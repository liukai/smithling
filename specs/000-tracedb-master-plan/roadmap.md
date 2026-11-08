# tracedb master roadmap (plan of record)

> This is the master plan and progress tracker for tracedb. Every other doc points here for status and decisions.
> Changes of direction (what we decided before, and why it changed) are logged in [decisions.md](decisions.md).
> Requirements: [spec.md](spec.md) · Architecture and phases: [plan.md](plan.md) · Design: [design-notes.md](design-notes.md) ·
> Principles: [constitution](../../.specify/memory/constitution.md). Each phase is its own spec (`specs/00N-…`); this file rolls them up.

**Goal / Done when**: `tracedb demo` passes every scenario check and the speed targets (spec.md SC-001–SC-010) on a laptop
with ~2M runs, and an unmodified LangGraph agent traces into it.

**Scope**: spec.md Stories 1–7 and 9–12. Not in scope: backlog B1 (delete/retention), Vortex, OTLP. (The custom search index left this list when M3 built it, D-026; the cloud split left it as M5, D-036.)

**Stakes**: low (local, reversible). Nothing ships outside this machine.

**Standing preferences**: Rust; as close to SmithDB as possible; keep the design simple; UI first; Scuba/Honeycomb-style
querying; describe each work group as goal + why → end state as "As a <who>, I can <what>" → work tied to the end state.

**Decisions & waived gates** (dated):
- 2026-11-05: Build in **Rust**, with an architecture **as close to SmithDB as possible** (constitution Principle II, v1.0.0).
- 2026-11-05: UI first; every phase ships its browser view (spec.md FR-025).
- 2026-11-05: Delete and retention are backlog (spec.md B1).
- 2026-11-05: **Keep the design simple.** Pick the plainest option that meets the spec; anything heavier goes to the backlog.
- 2026-11-05: **Vortex and the custom search index are backlog.** v0 uses Parquet and scan + per-segment bloom search, each behind an interface. *(Search part superseded by [D-026](decisions.md#d-026): M3 built a per-segment text index behind the `TextIndex` trait; Vortex stays backlog.)*
- 2026-11-05: **Ingestion, query and compaction are separate services** with explicit interfaces, in one process (design-notes §2.1).
- 2026-11-05: **Hot-data cache stays in memory locally**, behind a `Cache` interface. *(Today `Cache` is a concrete struct in storage, not a trait: [D-013](decisions.md#d-013), [D-028](decisions.md#d-028).)*
- 2026-11-05: **UI querying is Scuba/Honeycomb-style** (spec.md FR-029–032).
- 2026-11-05: **Each phase is its own spec**. `000-tracedb-master-plan` is the master (requirements, architecture, roadmap), with no code tasks. Phase specs are numbered from 001.
- 2026-11-05: **Phase details live in the phase folder.** Any new change to M1 goes into `specs/001-m1-see-agent-trace-live/`; this roadmap only links to it and tracks status.
- 2026-11-05: **LangSmith JSON ingest is in M1** (`/info`, `/runs/batch`, `POST/PATCH /runs`), so the M1 demo uses a real agent. Multipart stays in M3.
- 2026-11-05: **Merge rule**: the latest value wins for every field; nothing appends (research R1, because the SDK re-sends full values).
- 2026-11-05: **Constitution v1.1.0**: SmithDB is the target reached in stages; "deliberately staged" is an allowed reason (Principle II (c)). No Simplicity principle; simplicity stays a standing preference.
- 2026-11-05: **M1 reconciliation with SDK 0.14** (tasks T066–T069): `address` in the model; overload codes `429`+`Retry-After` (queue full), `507` (memory budget, whole request), `503`+`Retry-After` (storage); `POST /feedback` → `501`; `/info.version` empty. *(`507` superseded by [D-006](decisions.md#d-006): removed in M2; `501` superseded by [D-027](decisions.md#d-027): feedback is stored in M3.)*
- 2026-11-05 (standing approval): **proceed through M1 implementation**, committing each PR locally once its checks pass; no pushes. Report per PR.
- 2026-11-05: **RocksDB ideas in the backlog**: pinned reader versions, write slowdown, key bloom filters, two memtables (Backlog R-7–R-10). Rate limit, blob GC, checkpoints, merge operator, FIFO compaction not adopted. *(R-7–R-10 were built in M2.)*
- 2026-11-06: **Milestones numbered from 1**: M0–M4 renamed M1–M5 everywhere (docs, code comments, messages, phase folder `001-m1-…`, branch). Commit messages before this keep the old labels.
- 2026-11-06: **Every phase ends with a code starter guide** (`starter-guide.md` in the phase folder): reading order, one run traced through the code with line-level links. Planning includes a pre-code version (Appendix A of the plan) with pseudo-flows.
- 2026-11-06: **Diagrams read top to bottom**: prefer vertical (`flowchart TB`) charts with short labels and few nodes per chart; no wide horizontal chains.
- 2026-11-06: **Mark the delta in every phase spec and starter guide**: 🟢 New (green fill), 🟠 Improved / revised (orange), ⚪ Existing (grey) on end-state lines, lifecycle rows, stories, requirements, success criteria and every diagram node, plus a "Delta at a glance" section near the top of the spec.
- 2026-11-06 (M3 scope): **multipart + zstd** accepted and advertised in `/info` (SDK defaults work unchanged; [D-030](decisions.md#d-030)); **attachment parts rejected per part** with a named reason (stored later; [D-031](decisions.md#d-031)); **text index without word positions** (staged SmithDB difference, revisit if text-search p99 misses); **M2 = built, passed locally** (browser and cloud checks open), M3 branches from the 002 tip.
- 2026-11-06 (M3 plan): **memory defaults may change** to meet 200 MB idle (64 MB memtable measured as heap, 2 min flush age, 16 B dedup fingerprints with a cap, 32 MB cache, jemalloc; all configurable; heap-profile spike first); **feedback check added to `tracedb demo`**; **CI bench**: 200k smoke on PRs, full 2M on main + nightly; plan.md defaults accepted (four changed decisions to be logged, `feedback.<key>` = mean score, batch a now−30 h / b now−6 h, stats line `by <key> | <metrics>`, background index of M2 files, report rather than loosen a missed 1 s grouped target).
- 2026-11-06 (standing approval): **implement all of M3** (specs/003 tasks), on branch `003-m3-search-aggregates-speed` cut from the 002 tip after the last M2 commit; M3 spec/plan/tasks committed as the branch's first commit; one local commit per plan unit, same per-unit quality bar as M2, no pushes; the last unit rewrites `specs/003-…/starter-guide.md` for the built code, colour-coded.
- 2026-11-06 (standing approval): **implement all of M2** (tasks T001–T083), one local commit per plan unit, no pushes; the last step rewrites `specs/002-…/starter-guide.md` for the built code.
- 2026-11-06: **Per-unit quality bar** (solo project): tests first and failing, then green; property tests for risky logic; `fmt` + `clippy -D warnings` + full `cargo test` + UI `npm test`; a fresh-eyes reviewer pass on the unit's diff, serious findings fixed before commit; docs/tasks updated in the same commit; honest report. Slow suites + quickstart steps at the four M2 checkpoints. Milestone branch, one commit per unit; real PRs once there's a remote.
- 2026-11-06: **Commit identity**: the personal email set in this repo's git config; no `Co-Authored-By: Claude` / "Generated with Claude Code" lines. History rewritten to match (backup tags `backup/pre-rewrite/*`).
- 2026-11-06: **Keep 000 in step with reality**: after every phase checkpoint, update the roadmap (STATE, NEXT, progress), the [decision log](decisions.md) for any change of direction, and the master plan/design notes where the design moved. The decision log records every change of an earlier decision.
- 2026-11-06: **After M2 is built, spec M3 the same way as 002** (spec → plan → tasks via speckit, Appendix A starter guide); M3 docs stay uncommitted until the user approves them.
- 2026-11-07: **Commit each unit as soon as it passes its checks**, without waiting for other units that are still running (split shared files by hunk; verify the staged copy on its own).
- 2026-11-07 (waived gate): **M3 idle memory (QR-006 (M3 SC-05), ≤ 200 MB) is waived**: M3 ships at ~234–246 MB idle at 0.5M runs (~0.6 GB projected at 2M); the memory is merged copies of late-updated runs and per-file state, which **M4 compaction** removes (research M3-R12, D-028). Disk (≤ 50%) is met.
- 2026-11-07: **Text search is benchmarked as three rows** (rare phrase, common word, search + filter), each held to median ≤ 500 ms and p99 ≤ 5× its own median, as M2 measured them ([D-032](decisions.md#d-032)).
- 2026-11-07: **The full 2M-run quiet bench (T109) moves to the backlog** (D3-1): not practical on this machine; the bench code, its three search rows and the waived idle row ship in M3, and the nightly CI job runs the full bench once a remote exists ([D-032](decisions.md#d-032)).
- 2026-11-07 (priority): **A working end-to-end version comes first; performance is not a concern for now.** Speed targets, the 2M bench and memory (D3-1, D3-2, the parked branch `wip/m3-bench-three-rows`, *superseded by `ea6af31`, which merged the three-row bench*) are deferred, not dropped. **Re-evaluate before M4 starts** (or once the end-to-end version is in use): resume or re-scope D3-1/D3-2 then ([D-033](decisions.md#d-033)).
- 2026-11-08: **The `tracedb` crate is split** into `tracedb-server` (production surface), `tracedb-tools` (seed, demo, bench, CLI query) and the `tracedb` binary with a default `tools` feature; `--no-default-features` builds serve only; the shared test server is `tracedb_server::testkit` ([D-034](decisions.md#d-034)).
- 2026-11-08: **Interfaces stay as built** ([D-035](decisions.md#d-035)): only `TextIndex` is a trait; catalog, cache and segment format stay concrete types until a second implementation is needed. Don't add traits yet.
- 2026-11-08: **New milestone M5 — Disaggregated trace database**, after M4 compaction and before the agents milestone (now **M6**): ingestion, read workers and compaction as separate services over shared storage and catalog, one binary ([D-036](decisions.md#d-036); spec `specs/005-m5-disaggregated-trace-db/`, folder `004-` reserved for M4). Spec and plan only for now; no implementation.

**STATE as of 2026-11-08** (verified on branch `refactor/split-tracedb-crates` at `4acfcdf` by `cargo test --workspace`
303 passed / 0 failed / 7 ignored, clippy `-D warnings`, `cargo fmt --check` and `tracedb demo --seed 7` 10/10; `npm test` and the
crash harness were last run at `22ca7c8` on 2026-11-07 and not re-run):
M2 is **built, passed locally**. M3 is **built, passed locally** and on `master` (`ac69c4e`; 14 plan units; 123/128 tasks (T121, T124 unchecked to re-evaluate later)):
text search over inputs, outputs and errors (31 hits, all `fetch_url`); aggregates, time series, distribution and batch
comparison (`fetch_url` 14.0% / 31 s vs 2.0% / 9 s; $4.12 vs $4.07); progressive estimates; SDK multipart + zstd on
defaults; feedback stored, filtered and averaged; Stats/charts/Compare UI; `tracedb demo` checks M2 + M3 (also with
`--url`, D3-8 closed); `tracedb bench` built with three text-search rows. The crate split ([D-034](decisions.md#d-034))
is on `refactor/split-tracedb-crates` (`028bd38`, `4acfcdf`), **not yet merged to `master`**. **Deferred by decision**
(end-to-end first, performance later, [D-033](decisions.md#d-033)): the full 2M bench (D3-1) and idle memory ≤ 200 MB
(D3-2). **Not verified here**: cloud CI (D3-3), the UI in a real browser (D3-4), no-outbound under strace (D3-5).

**Blocked on**: nothing.

**NEXT**: 1) Merge `refactor/split-tracedb-crates` into `master`. The CI workflow now triggers on `master` as well as
`main` (`.github/workflows/ci.yml`, `branches: [main, master]`). 2) Push to a remote so CI (incl. cloud and the nightly
bench) runs; the user looks at the UI in a browser (D3-4). 3) **Before M4: re-evaluate the deferred performance work**
(D3-1, D3-2; only the 2M run itself remains), then plan M4.

## Progress

| Phase | Spec | Status | Tasks done | Exit check |
|---|---|---|---|---|
| M1 See agent trace live | [`001-m1-see-agent-trace-live`](../001-m1-see-agent-trace-live/spec.md) | built; exit check passed locally except the UI by eye and CI-only checks | 69/69 | passed locally (see STATE) |
| M2 Durable storage + query builder | [`002-m2-durable-storage-query-builder`](../002-m2-durable-storage-query-builder/spec.md) | built; passed locally except the UI by eye and CI-only checks | 83/83 | passed locally (see STATE) |
| M3 Core demo | [`003-m3-search-aggregates-speed`](../003-m3-search-aggregates-speed/spec.md) | built; passed locally; performance deferred (D3-1, D3-2) | 123/128 | demo 10/10 + quickstart; QR-004/005/006 deferred (D3-1, D3-2) |
| M4 Follow-through | — | not started | — | — |
| M5 Disaggregated trace database | [`005-m5-disaggregated-trace-db`](../005-m5-disaggregated-trace-db/spec.md) | spec drafted (3 open questions) | — | — |
| M6 Zero instrumentation + agents | — | not started | — | — |

"Tasks done" counts `[X]` in each phase's `tasks.md`. A phase is **done** only when its exit check passes, not when its PRs merge.

## Phases

### M1: See agent trace live

> Details of record: [`specs/001-m1-see-agent-trace-live/`](../001-m1-see-agent-trace-live/spec.md). The summary below is the original scope; the phase folder wins if they differ.

**Goal and why**: prove the whole path (agent → ingest → durable log → query → browser) works end to end, thinly.
It **unblocks** every later phase: M2 puts storage under the same API, and M3 hardens the same ingest. It also gives
the UI a real data source from day one (UI first).

**End state** (capability first; *Example* = the scenario we prove it with):
- As an **agent developer**, I can trace any agent built on the LangSmith SDK (LangChain, LangGraph, or plain
  `@traceable` code) into tracedb by changing only its endpoint setting, and see each step in the browser within 1 s.
  Steps still running show as `running` and update when they finish, however long that takes.
  - *Example*: the research agent from the scenario, with a 40-minute root that flips from `running` to `ok`.
- As an agent developer, I can open any trace and see its full tree and timing waterfall at once, then click any step to read its complete inputs and outputs.
  - *Example*: a 250-step trace; clicking `fetch_url` shows its 40 KB response.
- As a **test user** without an agent, I can generate realistic traces (deep trees, late-finishing steps, large payloads) and explore them in the browser.
  - *Example*: `tracedb seed --traces 100`.
- As an agent developer, I can have tracedb crash or be killed at any moment and lose nothing it acknowledged.
- As a **maintainer**, I can trust all of the above stays true: CI runs the crash test (10 kill -9 rounds), the SDK contract test and the zero-outbound check on every PR.

**Work needed → how it gets us there**:
| Work | Contribution |
|---|---|
| Workspace + `model` crate (events, runs, merge rule, `dotted_order`, derived status/latency) | Direct: what the browser shows is the merged run. Indirect: shared by every service in every phase |
| WAL with group commit + memtable + replay | Direct: the kill -9 check. Indirect: M2 flushes this memtable to disk |
| Ingest: `/v1/events`, `/info` (advertises no multipart), `/runs/batch`, `POST/PATCH /runs` | Direct: an unmodified agent traces in (the headline demo) |
| Query: trace tree, single run, root-runs list with cursor, live tail | Direct: the runs list and trace view have data. Indirect: M2 swaps in DataFusion behind the same API |
| UI: runs list (live), trace tree + waterfall, run panel, empty state, shareable URLs | Direct: the demo is in the browser |
| `tracedb seed` (deep trees, late ends, big payloads) | Direct: the 250-run demo. Indirect: realistic data for UI work and M2 scale tests |
| Crash test, no-outbound check, SDK contract test in CI | Durable: keeps the end state true after M1 |

**Delivers**: FR-001 (JSON path), FR-002–007, FR-012 (basic), FR-022 (list + trace), FR-025, FR-027 (partial); QR-001, QR-002, QR-003.

### M2: Durable storage + query builder

**Goal and why**: hold real volumes on disk and let people find things by clicking. This **unblocks** M3's speed and
search work, and delivers the first half of morning triage (scenario steps 4, 5, 8).

**End state** (capability first; *Example* = the scenario we prove it with):
- As an **agent developer**, I can narrow all my runs down to the ones I care about by time, tags, status, latency, type, name and any metadata or JSON field, using a point-and-click query builder with no syntax to learn.
  - *Example*: last night's eval batch, root runs that errored → **14**.
- As an agent developer, I can find runs by what happened anywhere in their trace, not just in the run itself.
  - *Example*: root runs that succeeded but had a failing tool step somewhere inside → **23**.
- As an agent developer, I can see every field in my data with its most common values and counts, and filter on a value with one click.
  - *Example*: `prompt_version: v7 (912), v6 (928)` → click `v7`.
- As an agent developer, I can share any view as a link that reopens exactly the same query.
- As an agent developer, I can restart tracedb and keep all my data, at laptop scale (millions of runs).
- As a **test user**, I can generate the overnight-eval scenario and reproduce the example numbers above.
- As a **maintainer**, I can trust queries return correct results on local disk and on cloud storage: CI checks the scenario numbers on both.

**Work needed → how it gets us there**:
| Work | Contribution |
|---|---|
| Flush memtable → Parquet core/payload segments; SQLite metastore; replay-safe WAL truncation | Direct: data survives restart at scale |
| DataFusion scans with pruning; merge-on-read (memtable ∪ segments) | Direct: queries answer over all data, including late patches |
| Filter DSL + tree-aware filters + cursor paging | Direct: the 14 and 23 counts |
| Field catalog | Direct: field browser and value suggestions |
| UI query builder (panel ↔ text, chips), field browser, Samples + Table views, click-to-filter | Direct: triage with no syntax (SC-010) |
| `tracedb demo` (first checks); MinIO + Postgres CI job | Durable: the counts and cloud portability stay true |

**Delivers**: FR-008–010, FR-012, FR-013, FR-026, FR-029–031 (partial); QR-007.

### M3: Core demo

**Goal and why**: finish what the overnight-eval story needs (steps 1–9) at full speed. This is **the v0 headline**
("is this a pattern? how bad?").

**End state** (capability first; *Example* = the scenario we prove it with):
- As an **agent developer**, I can search the text of all inputs and outputs for a phrase and combine that with any other filter.
  - *Example*: `"429 Too Many Requests"` → **31** runs, all `fetch_url`.
- As an agent developer, I can aggregate any set of runs (count, error rate, latency percentiles, tokens, cost), grouped by any field, as a table or charts.
  - *Example*: grouped by tool, `fetch_url` shows **14%** errors and p95 **31 s**; the batch cost **$4.12**.
- As an agent developer, I can compare two sets of runs side by side or overlaid on the same chart.
  - *Example*: batch a vs. batch b after a retry fix.
- As an agent developer, I can use the current LangSmith SDK with its default settings, with no configuration beyond the endpoint.
- As an agent developer, I can get every one of these answers at interactive speed with millions of runs on a laptop (the targets are in spec QR-004/005/008).
- As a **test user**, I can run one command that replays the whole scenario and shows each check passing, and another that prints the speed table.
- As a **maintainer**, I can trust the speed claims: CI runs the demo and benchmark, and fails if a target slips.

**Work needed → how it gets us there**:
| Work | Contribution |
|---|---|
| LangSmith multipart + feedback parts | Direct: current SDK defaults work unchanged |
| Payload split + late materialization; progressive query; cache | Direct: speed targets for tree, run and filter |
| Text search behind `TextIndex` (planned as bloom + scan; built as a per-segment text index, D-026) | Direct: the 31 hits |
| Aggregations; Time series + Distribution views; comparison overlay | Direct: 14% / 31 s / $4.12, and batch a vs b |
| `tracedb bench` | Durable: the speed claims are measured, not asserted |

**Delivers**: FR-001 (full), FR-011, FR-014, FR-015, FR-023, FR-024, FR-031; QR-004, QR-005, QR-006, QR-008.

### M4: Follow-through

**Goal and why**: close the loop (steps 10, 11, 13) and keep old data cheap. It moves the scenario from "found it" to
"fixed it and prevented it".

**End state** (capability first; *Example* = the scenario we prove it with):
- As an **agent developer**, I can view a multi-turn conversation that spans several traces as one chat thread, in order.
  - *Example*: a task with **6** turns.
- As an agent developer, I can export any set of runs with their inputs, outputs and metadata to a file I can reuse as an eval dataset.
  - *Example*: the **37** bad root runs to JSONL.
- As an agent developer, I can select an unusual part of a chart and see which fields and values set it apart, ranked.
  - *Example*: the error spike → `fetch_url` and `prompt_version = v7` at the top.
- As an agent developer, I can rely on runs that finish late showing correctly, no matter when their last update arrives or whether background compaction has run.
- As an agent developer, I can add a score and comment to any finished run from the UI, then filter and average by it (filtering and averaging, and SDK feedback, shipped in M3).
- As a **maintainer**, I can trust these keep working: CI covers threads, export, compaction correctness and the ranking example.

**Work needed → how it gets us there**:
| Work | Contribution |
|---|---|
| Compaction tiers + atomic swap | Direct: late-end correctness after compaction. Indirect: keeps M3 speeds as data grows |
| Threads view; export; feedback write from the UI | Direct: steps 10, 11 and feedback from the browser |
| Find-what-stands-out | Direct: Story 12 |

**Delivers**: FR-016, FR-017, FR-020, FR-032.

### M5: Disaggregated trace database

> Details of record: [`specs/005-m5-disaggregated-trace-db/`](../005-m5-disaggregated-trace-db/spec.md); code check of the design note: [review.md](../005-m5-disaggregated-trace-db/review.md).

**Goal and why**: run ingestion, read serving and compaction as separate services that scale on their own, over shared
durable storage. Multi-day agent traces and M6's read-heavy agent investigation outgrow one process; the storage layout
is already cloud-ready (constitution V), so the work is between processes.

**End state** (capability first; *Example* = the scenario we prove it with):
- As an **operator**, I can run ingest, read and compaction services from the one binary against shared storage and catalog, and get the same answers as one-process mode.
  - *Example*: 1 writer, 2 readers, 1 compactor; `tracedb demo` passes with identical counts.
- As an operator, I can add or restart read workers without touching ingestion; each replays the durable log and reports its lag.
- As an **agent developer**, I see acknowledged steps on any reader within seconds, never twice and never missing across flushes and compactions.
- As an operator, I can hand a shard to a new writer; the old one is fenced out.
- As a **maintainer**, I can trust it: CI runs the split demo, the new crash points and the scaling bench.

**Work needed → how it gets us there**:
| Work | Contribution |
|---|---|
| Per-shard durable log, positions, retention; ack contract | Direct: readers replay; acknowledged writes survive the promised failures |
| Per-shard coverage + pinned snapshots + leases in the catalog | Direct: no missed or doubled runs across flush and compaction |
| Read workers (own tail, bootstrap, lag, wait-for-position) | Direct: independent read scaling |
| Compaction pool out of process; safe orphan sweep | Direct: compaction never slows ingest or breaks a query |
| Ownership epochs, fencing, handoff, routing | Direct: one writer per shard, always |
| Cluster page; split-mode demo, crash points, bench in CI | Durable: keeps the end state true |
| README and crate descriptions: the tagline gains its second half ("…locally, or scale out when you need to") | Direct: the project says what it now does |

**Delivers**: the cloud split (master Scope, D-036); constitution Architecture Constraints; backlog R-6.

### M6: Zero instrumentation + agents

**Goal and why**: reach the second audience (Claude Code users) and the long-term goal: agents debugging agents.

**End state** (capability first; *Example* = the scenario we prove it with):
- As a **Claude Code user**, I can import my local session history and use every triage feature on it, with no instrumentation.
  - *Example*: "which session ran the failing `cargo test`, and what did the agent do before that?"
- As an **AI agent**, I can investigate traces through tools sized for my context (outline, error path, payload slices, failure clusters, diffs) and write my findings back onto runs.
  - *Example*: answer "why did batch a fail?" by naming the 429 cause and citing the 31 runs.
- As an agent developer, I can follow an agent's cited links straight to the runs it used, and check its work.
- As a **maintainer**, I can trust the importer and agent tools stay working: CI runs both on fixtures.

**Work needed → how it gets us there**:
| Work | Contribution |
|---|---|
| Claude Code importer (idempotent) | Direct: Variant B |
| Agent tools (outline, error path, sliced payloads, clusters, diff, annotate) + `tracedb mcp` | Direct: Story 11 |

**Delivers**: FR-021, FR-028.

## Requirement coverage

| Requirement | Phase | Done |
|---|---|---|
| FR-001 | M1 (JSON), M3 (multipart) | JSON path, multipart + zstd verified (contract fixtures) |
| FR-002–007 | M1 | built, verified locally |
| FR-008–010, 012, 013 | M2 | built, verified locally |
| FR-011, 014, 015 | M3 | built, verified locally |
| FR-016, 017, 020 | M4 | |
| FR-021, 028 | M6 | |
| FR-022–027 | M1 → M3 (grows each phase) | built through M3, verified locally; browser check open (D3-4) |
| FR-029–031 | M2 (builder, Samples, Table) → M3 (charts, compare) | built, verified locally |
| FR-032 | M4 | |
| QR-001–003 | M1 | QR-003 verified (crash 10/10); QR-001/002 CI-only |
| QR-004–006, 008 | M3 | deferred (D3-1, D3-2); QR-006 disk met |
| QR-007 | M2 | built, verified locally; cloud CI not run (D3-3) |

Mark ✅ only after the phase's exit check passes.

## Backlog: SmithDB reconciliation (found 2026-11-05)

From checking spec 000 against the SmithDB post, the LangSmith self-hosted docs, and `langsmith` SDK 0.8.18 vs 0.14.17.
First reconciliation round applied to the docs on 2026-11-05 (source read from `/tmp/lsdiff/`); the code changes are proposed as M1 tasks.
**Local** = does the laptop version need it.

| # | Item | Local? | When | Status (2026-11-05) |
|---|---|---|---|---|
| R-1 | Re-pin the SDK to Python **0.14.17** (keep 0.8.18 as a second fixture set). Accept the new `address` field (`lrn:agents/<id>/environments/<env>`); when no project is named the SDK drops `session_name`, so derive the project from the address | **Yes** | M1 | **Docs updated** (000 spec, design-notes §4, research R2, data-model; 001 spec, data-model, design §6/§9, research, tasks T030/T031/T034/T061). Verified in SDK source. Code: proposed tasks |
| R-2 | ~~Replace the `eq()/and()` grammar with `field:value`~~. **Corrected:** SDK 0.14.17 still documents `eq()/and()/has()` for `filter`, so it stays canonical. A `field:value` search-bar form is a possible M2 add-on once confirmed in LangSmith's UI docs | Maybe | M2 | **Docs updated** (design-notes §5.2). `field:value` unverified offline |
| R-3 | design-notes §6 wording (term zones; postings/positions chunked apart from the term dictionary) and SmithDB ingest numbers (P50 630 ms / P99 1.47 s) in §2 | Doc only | Anytime | **Docs updated**, from the earlier reconciliation note; the SmithDB post was not re-read in this session |
| R-4 | Pin `object_store` to 0.13 (`vortex-datafusion` 0.91 needs 0.13.2; latest is 0.14.2); Vortex reason → "pre-1.0, not benchmarked by us; supports DataFusion 55" | Cheap | M2 (when crates are added) | **Docs updated** (plan, research R4, design-notes §12). Verified on crates.io. `Cargo.toml` pin when `object_store` is added |
| R-5 | If we add LangSmith *read* compatibility, copy the v2 API (`/api/v2/runs/query`, `/api/v2/threads/query`, project UUIDs, no child runs in run queries); v1 is removed on Cloud 2027-01-31 | No: compat is ingest-only | Deprioritized | No change |
| R-6 | Cloud path: Compaction → one scheduler + scalable workers; cluster manager as its own service (self-hosted SmithDB runs 5 services) | No | Cloud | **Docs updated** (design-notes §2.1) |

### LSM ideas borrowed from RocksDB / LevelDB (chosen 2026-11-05)

| # | Item | Why | Done when | Status |
|---|---|---|---|---|
| R-7 | **Pinned versions for readers**: each query holds a reference to the segment set it started on; a compacted-away segment is deleted only when no reader holds it (replaces the grace-period GC in design-notes §7) | A long query never loses a file mid-scan | Test: a scan runs across a compaction swap and returns the same rows | built (M2) |
| R-8 | **Write slowdown**: when memtable size, unflushed WAL or T0 file count passes a limit, ingest returns `429` + `Retry-After` (the SDK retries) | Memory stays bounded during bursts | Test: a burst past the limit gets 429s, no event lost, memory under the cap | built (M2) |
| R-9 | **Bloom filter on keys**: Parquet bloom filters on `trace_id` and `run_id` in core files | Trace-tree and single-run lookups skip most segments | Bench: segments opened per lookup drops vs. without | built (M2) |
| R-10 | **Two memtables**: one mutable, one frozen while it flushes; writes go to the new mutable one | Ingest never pauses during a flush | Test: ingest latency stays flat while a flush runs | built (M2) |

Not adopted: compaction I/O rate limit, payload-file GC (BlobDB-style), checkpoints, merge operator, FIFO/TTL compaction.

### Not done in M3 (deferred 2026-11-07)

Work M3 planned but didn't finish or couldn't verify here. Each item says what would close it.

| # | Item | Why not done | Done when |
|---|---|---|---|
| D3-1 | **Full 2M-run bench on a quiet machine** (T109) | A clean 1–2 h run on a dedicated machine isn't practical here; the busy-machine run missed run + payload, filtered list, text search p99 and load (before unit 12a's fixes) | Bench code done (three text-search rows; idle row reported "waived (M4)", merged in `ea6af31`); `tracedb bench --runs 2000000 --check` passes every non-waived row on a quiet machine or the nightly CI job, table recorded in M3 research |
| D3-2 | **Idle memory ≤ 200 MB** (QR-006 (M3 SC-05), T087) | Waived for M3 (D-028): 234–246 MB at 0.5M runs, ~0.6 GB projected at 2M, held by merged copies of late-updated runs and per-file state | M4 compaction merges late updates; idle test green at 2M |
| D3-3 | **Cloud CI job's first real run** (T122, and the S3/Postgres storage tests T121), incl. M2's | No Docker, MinIO, Postgres or git remote on the build machine | The `cloud` job passes on a remote CI |
| D3-4 | **UI checked in a real browser** (M1, M2, M3 views) | No headless browser in the sandbox; covered by component tests and API smoke tests only | Someone clicks through quickstart step 2 and the Stats/charts/Compare views |
| D3-5 | **No-outbound check under strace** (T124) | Needs Linux; built on macOS | `scripts/no-outbound.sh` passes on Linux CI |
| D3-6 | Chart metric and chosen group kept in the URL | Not in the URL contract | A shared link reopens the same chart metric and group |
| D3-7 | Aggregate over a search stops early / cancels | The whole search runs before any estimate; closing the stream doesn't stop it | Estimate within 1 s with a search filter; cancel frees the scan |
| D3-8 | ~~`tracedb demo --url` against a default `serve` fails "checks across memtable and files"~~ **Done 2026-11-07**: `/healthz` reports `memtable_limit_bytes`; `demo --url` sizes its load from it (10/10 against a default serve in ~5 s) | — | — |
| D3-9 | Unreadable text-index sidecar is reported and rebuilt | A *missing* sidecar is counted in `/healthz` (`unindexed_segments`) and backfilled; an *unreadable* one only logs a warning and the file is scanned on every search, never counted or rewritten (`ingest/src/backfill.rs`, `query/src/search.rs`) | An unreadable sidecar shows in `/healthz` and backfill writes a new one |
| D3-10 | *(low priority)* Drop the special case that keeps the newest flush's `dedup_keys` row | The row was kept so start could rebuild the multi-version set from saved run ids; D-025 removed that use, so the row is now kept for no reason (one small row, harmless). Kept as is for now, with a code comment | The cleanup deletes every row older than the dedup window, with a test; or the comment is updated if a new use appears |

### Ideas not in the system (added 2026-11-08)

Not scheduled. Each names what's missing today and what would trigger the work; details go in a phase spec if
picked up.

| # | Idea | Today | Pick up when |
|---|---|---|---|
| I-1 | **Shred hot JSON paths into typed columns** (the Parquet Variant / ClickHouse JSON idea): the most-used `metadata.*` and output keys get their own column; the rest stays in `attrs` | promoted leaves live in one JSON text column (D-012); filters on them read the whole column (5.5 MB vs 211 KB for typed columns in a 1M-row test) and statistics can't skip anything | metadata filters show up as slow in the bench, or data moves to S3 |
| I-2 | **Cost-based condition order**: use the catalog's field stats (counts, top values) to run the most selective condition first, in the scan and in the evaluator | DataFusion orders pushed conditions by column size (`reorder_filters`); tracedb's evaluator keeps the written order (except `search` last) | filters with several conditions miss their targets |
| I-3 | **Reorder run, root and tree conditions by cost** | fixed order: run filter, then root, then tree (in candidate chunks) | tree queries dominate latency |
| I-4 | **Query-result cache** for repeated identical queries (dashboards, live views re-asking) | the cache holds file pieces (footers, pages), not answers | repeated queries show in profiles |
| I-5 | **Benchmark Vortex against Parquet** on the 2M-run data (SmithDB's file format; `vortex-datafusion` 0.93 fits DataFusion 55 / `object_store` 0.13) | Parquet only; no format trait (D-035) | run lookup or payload reads miss targets in D3-1 |
| I-6 | **Bound attribute promotion for wide JSON**: give `metadata` a leaf budget like `inputs` / `outputs` (64), and past any budget store one "not indexed" marker per root instead of one per key | `metadata` has no leaf budget, and every leaf past the 64-leaf budget of `inputs` / `outputs` still adds a `null` entry with its full path (`model/src/attrs.rs`), so `attrs` grows with the width of the JSON | runs with very wide metadata or outputs show up, or core files grow faster than expected |

### Scale, scalability and failure modes (to think through, added 2026-11-08)

Not designed yet; a checklist of basic ideas to start from.

**Scale**: what grows, and what grows with it.

| Grows | Pressure it puts on |
|---|---|
| runs per day, late updates | memtable, WAL, file count (until compaction), multi-version set memory |
| payload size (MB prompts) | payload files, text index, upload body limits |
| projects | files per flush (one segment per project), catalog rows |
| days kept | catalog size, footer reads per query; retention (B1) bounds it |
| concurrent users and live views | query CPU, re-queries per change, SSE connections |

**Scalability**: how far one process goes, and what splits first.

- *One node*: the single writer and one frozen memtable cap ingest; file count caps queries until compaction (M4).
  Measure the ceilings first (the D3-1 bench) before splitting anything.
- *Scale out* (the SmithDB shape, design-notes §2; now planned as [M5](#m5-disaggregated-trace-database)): stateless query nodes over S3 + Postgres; ingest partitioned by
  project (each partition its own writer and WAL); compaction as separate workers; a cache tier (SSD) with sticky
  routing so a project's queries hit warm nodes.
- *Catalog*: one Postgres row per segment; compaction keeps the row count bounded.

**Failure modes**: what can go wrong, and the first answer.

| Failure | Today | First idea |
|---|---|---|
| disk full | writes fail; health turns false, uploads refused | a disk-space watermark that triggers `429` before the WAL write fails |
| WAL or data file corrupt | torn WAL tail cut on replay; damaged data files skipped and named (M3) | checksums on every file read path; a repair / re-index command |
| S3 slow or down (cloud) | flush retries with backoff (200 ms → 30 s), keeping the frozen memtable; data stays in the WAL; uploads get `429` once the next memtable fills | alert on unflushed-WAL growth; a read path that tolerates slow S3 (timeouts, hedged reads) |
| catalog (Postgres) unavailable | the flush commit retries the same way; queries keep serving from the in-memory file list (D-020) | report it in `/healthz`; decide how long to keep accepting uploads |
| crash mid-flush / mid-compaction | orphans deleted on start (flush); compaction not built | same rule for compaction: files first, one catalog transaction, orphans swept |
| memory pressure | memtable cap + `429`; idle memory over target until M4 | memory budget across memtable, cache, multi-version set; shed live views first |
| huge or malformed uploads | per-field and body caps, per-item rejection | per-project quotas |
| clock skew in client timestamps | order by arrival `seq`, not client time | flag runs whose times are far off |

## History

- 2026-11-05: product spec, primer, design notes, constitution v1.0.0, plan + Phase 1 artifacts written; the original SPEC.md and REQUIREMENTS.md were folded in and deleted.
- 2026-11-05: M1 implemented (PR1–PR7). Found by recording real SDK traffic: `/info` must send every `batch_ingest_config` key (the SDK reads them without defaults), and an invalid `LANGSMITH_AGENT_ENVIRONMENT` makes the SDK skip tracing silently.
- 2026-11-06: M2 built and passed locally (83/83 tasks; `c7595c6`).
- 2026-11-07: M3 built and passed locally (125/128 tasks; `001d878`); three-row text-search bench, idle row waived, and `demo --url` fix (D3-8) merged to `master` at `ac69c4e`.
- 2026-11-08: `crates/tracedb` split into `crates/server`, `crates/tools` and the binary (`028bd38`, D-034); newcomer starter guide and README layout (`4acfcdf`). On branch `refactor/split-tracedb-crates`, not yet on `master`.
- 2026-11-08: M5 Disaggregated trace database inserted after M4; agents milestone renumbered M6 (D-036). Spec drafted in `specs/005-m5-disaggregated-trace-db/`.
