# tracedb master roadmap (plan of record)

> This is the master plan and progress tracker for tracedb. Every other doc points here for status and decisions.
> Changes of direction (what we decided before, and why it changed) are logged in [decisions.md](decisions.md).
> Requirements: [spec.md](spec.md) · Architecture and phases: [plan.md](plan.md) · Design: [design-notes.md](design-notes.md) ·
> Principles: [constitution](../../.specify/memory/constitution.md). Each phase is its own spec (`specs/00N-…`); this file rolls them up.

**Goal / Done when**: `tracedb demo` passes every scenario check and the speed targets (spec.md SC-001–SC-010) on a laptop
with ~2M runs, and an unmodified LangGraph agent traces into it.

**Scope**: spec.md Stories 1–7 and 9–12. Not in scope: backlog B1 (delete/retention), Vortex, the custom search index, OTLP, the cloud split.

**Stakes**: low (local, reversible). Nothing ships outside this machine.

**Standing preferences**: Rust; as close to SmithDB as possible; keep the design simple; UI first; Scuba/Honeycomb-style
querying; describe each work group as goal + why → end state as "As a <who>, I can <what>" → work tied to the end state.

**Decisions & waived gates** (dated):
- 2026-11-05: Build in **Rust**, with an architecture **as close to SmithDB as possible** (constitution Principle II, v1.0.0).
- 2026-11-05: UI first; every phase ships its browser view (spec.md FR-025).
- 2026-11-05: Delete and retention are backlog (spec.md B1).
- 2026-11-05: **Keep the design simple.** Pick the plainest option that meets the spec; anything heavier goes to the backlog.
- 2026-11-05: **Vortex and the custom search index are backlog.** v0 uses Parquet and scan + per-segment bloom search, each behind an interface.
- 2026-11-05: **Ingestion, query and compaction are separate services** with explicit interfaces, in one process (design-notes §2.1).
- 2026-11-05: **Hot-data cache stays in memory locally**, behind a `Cache` interface.
- 2026-11-05: **UI querying is Scuba/Honeycomb-style** (spec.md FR-029–032).
- 2026-11-05: **Each phase is its own spec**. `000-tracedb-master-plan` is the master (requirements, architecture, roadmap), with no code tasks. Phase specs are numbered from 001.
- 2026-11-05: **Phase details live in the phase folder.** Any new change to M1 goes into `specs/001-m1-see-agent-trace-live/`; this roadmap only links to it and tracks status.
- 2026-11-05: **LangSmith JSON ingest is in M1** (`/info`, `/runs/batch`, `POST/PATCH /runs`), so the M1 demo uses a real agent. Multipart stays in M3.
- 2026-11-05: **Merge rule**: the latest value wins for every field; nothing appends (research R1, because the SDK re-sends full values).
- 2026-11-05: **Constitution v1.1.0**: SmithDB is the target reached in stages; "deliberately staged" is an allowed reason (Principle II (c)). No Simplicity principle; simplicity stays a standing preference.
- 2026-11-05: **M1 reconciliation with SDK 0.14** (tasks T066–T069): `address` in the model; overload codes `429`+`Retry-After` (queue full), `507` (memory budget, whole request), `503`+`Retry-After` (storage); `POST /feedback` → `501`; `/info.version` empty.
- 2026-11-05 (standing approval): **proceed through M1 implementation**, committing each PR locally once its checks pass; no pushes. Report per PR.
- 2026-11-05: **RocksDB ideas in the backlog**: pinned reader versions, write slowdown, key bloom filters, two memtables (Backlog R-7–R-10). Rate limit, blob GC, checkpoints, merge operator, FIFO compaction not adopted.
- 2026-11-06: **Milestones numbered from 1**: M0–M4 renamed M1–M5 everywhere (docs, code comments, messages, phase folder `001-m1-…`, branch). Commit messages before this keep the old labels.
- 2026-11-06: **Every phase ends with a code starter guide** (`starter-guide.md` in the phase folder): reading order, one run traced through the code with line-level links. Planning includes a pre-code version (Appendix A of the plan) with pseudo-flows.
- 2026-11-06: **Diagrams read top to bottom**: prefer vertical (`flowchart TB`) charts with short labels and few nodes per chart; no wide horizontal chains.
- 2026-11-06: **Mark the delta in every phase spec and starter guide**: 🟢 New (green fill), 🟠 Improved / revised (orange), ⚪ Existing (grey) on end-state lines, lifecycle rows, stories, requirements, success criteria and every diagram node, plus a "Delta at a glance" section near the top of the spec.
- 2026-11-06 (M3 scope): **multipart + zstd** accepted and advertised in `/info` (SDK defaults work unchanged); **attachment parts rejected per part** with a named reason (stored later); **text index without word positions** (staged SmithDB difference, revisit if text-search p99 misses); **M2 = built, passed locally** (browser and cloud checks open), M3 branches from the 002 tip.
- 2026-11-06 (M3 plan): **memory defaults may change** to meet 200 MB idle (64 MB memtable measured as heap, 2 min flush age, 16 B dedup fingerprints with a cap, 32 MB cache, jemalloc; all configurable; heap-profile spike first); **feedback check added to `tracedb demo`**; **CI bench**: 200k smoke on PRs, full 2M on main + nightly; plan.md defaults accepted (four changed decisions to be logged, `feedback.<key>` = mean score, batch a now−30 h / b now−6 h, stats line `by <key> | <metrics>`, background index of M2 files, report rather than loosen a missed 1 s grouped target).
- 2026-11-06 (standing approval): **implement all of M3** (specs/003 tasks), on branch `003-m3-search-aggregates-speed` cut from the 002 tip after the last M2 commit; M3 spec/plan/tasks committed as the branch's first commit; one local commit per plan unit, same per-unit quality bar as M2, no pushes; the last unit rewrites `specs/003-…/starter-guide.md` for the built code, colour-coded.
- 2026-11-07: **Commit each unit as soon as it passes its checks**, without waiting for other units that are still running (split shared files by hunk; verify the staged copy on its own).
- 2026-11-07 (waived gate): **M3 idle memory (SC-05, ≤ 200 MB) is waived**: M3 ships at ~234–246 MB idle at 0.5M runs (~0.6 GB projected at 2M); the memory is merged copies of late-updated runs and per-file state, which **M4 compaction** removes (research M3-R12, D-028). Disk (≤ 50%) is met.
- 2026-11-07: **Text search is benchmarked as three rows** (rare phrase, common word, search + filter), each held to median ≤ 500 ms and p99 ≤ 5× its own median, as M2 measured them.
- 2026-11-07: **The full 2M-run quiet bench (T109) moves to the backlog** (D3-1): not practical on this machine; the bench code, its three search rows and the waived idle row ship in M3, and the nightly CI job runs the full bench once a remote exists.
- 2026-11-06 (standing approval): **implement all of M2** (tasks T001–T083), one local commit per plan unit, no pushes; the last step rewrites `specs/002-…/starter-guide.md` for the built code.
- 2026-11-06: **Per-unit quality bar** (solo project): tests first and failing, then green; property tests for risky logic; `fmt` + `clippy -D warnings` + full `cargo test` + UI `npm test`; a fresh-eyes reviewer pass on the unit's diff, serious findings fixed before commit; docs/tasks updated in the same commit; honest report. Slow suites + quickstart steps at the four M2 checkpoints. Milestone branch, one commit per unit; real PRs once there's a remote.
- 2026-11-06: **Commit identity**: the personal email set in this repo's git config; no `Co-Authored-By: Claude` / "Generated with Claude Code" lines. History rewritten to match (backup tags `backup/pre-rewrite/*`).
- 2026-11-06: **Keep 000 in step with reality**: after every phase checkpoint, update the roadmap (STATE, NEXT, progress), the [decision log](decisions.md) for any change of direction, and the master plan/design notes where the design moved. The decision log records every change of an earlier decision.
- 2026-11-06: **After M2 is built, spec M3 the same way as 002** (spec → plan → tasks via speckit, Appendix A starter guide); M3 docs stay uncommitted until the user approves them.

**STATE as of 2026-11-07** (verified by `git log`, `cargo test --workspace` 145 passed, clippy `-D warnings`, `tracedb demo --seed 7` 5/5, crash harness 10/10, `bench_m2` at 2M runs):
M2 is **built, passed locally** on branch `002-m2-durable-storage-query-builder` (83/83 tasks, plan units 1–12 plus
fixes D-019–D-024). 2M-run bench: lists 13–51 ms, trace 6 ms, run 28 ms, suggestions 3 ms, disk 19% of raw JSON;
with 300+ overlapping files and late updates sent without a project, failing roots 52 ms and quiet failures 109 ms.
**Not yet verified**: the UI in a real browser; the cloud CI job and no-outbound check under strace (no Docker or
remote CI here). **Known gap**: queries inside a project that holds many unmerged late updates take seconds (4.5 s at
192k such rows) — M3 unit 12 / M4 compaction. M3 is **specced and planned** (`specs/003-m3-search-aggregates-speed/`,
128 tasks).

**Blocked on**: nothing.

**NEXT**: 1) Cut `003-m3-search-aggregates-speed` from the 002 tip; commit the M3 spec, plan and tasks. 2) M3 units
1, 2, 3 and 8 in parallel, then 5, 4, 6, 7, 9, UI, speed + bench, demo, starter guide. 3) Still open from M1/M2: the UI
by eye in a browser; push to a remote so CI (incl. cloud) runs.

## Progress

| Phase | Spec | Status | Tasks done | Exit check |
|---|---|---|---|---|
| M1 See agent trace live | [`001-m1-see-agent-trace-live`](../001-m1-see-agent-trace-live/spec.md) | built; exit check passed locally except the UI by eye and CI-only checks | 69/69 | passed locally (see STATE) |
| M2 Durable storage + query builder | [`002-m2-durable-storage-query-builder`](../002-m2-durable-storage-query-builder/spec.md) | built; passed locally except the UI by eye and CI-only checks | 83/83 | passed locally (see STATE) |
| M3 Core demo | [`003-m3-search-aggregates-speed`](../003-m3-search-aggregates-speed/spec.md) | specced and planned | 0/128 | — |
| M4 Follow-through | — | not started | — | — |
| M5 Zero instrumentation + agents | — | not started | — | — |

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
| Text search (bloom + scan) behind `TextIndex` | Direct: the 31 hits |
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
- As an agent developer, I can add a score and comment to any finished run, then filter and average by it.
- As a **maintainer**, I can trust these keep working: CI covers threads, export, compaction correctness and the ranking example.

**Work needed → how it gets us there**:
| Work | Contribution |
|---|---|
| Compaction tiers + atomic swap | Direct: late-end correctness after compaction. Indirect: keeps M3 speeds as data grows |
| Threads view; export; feedback write | Direct: steps 10, 11 and feedback filters |
| Find-what-stands-out | Direct: Story 12 |

**Delivers**: FR-016, FR-017, FR-020, FR-032.

### M5: Zero instrumentation + agents

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
| FR-001 | M1 (JSON), M3 (multipart) | JSON path verified (contract fixtures) |
| FR-002–007 | M1 | built, verified locally |
| FR-008–010, 012, 013 | M2 | |
| FR-011, 014, 015 | M3 | |
| FR-016, 017, 020 | M4 | |
| FR-021, 028 | M5 | |
| FR-022–027 | M1 → M3 (grows each phase) | |
| FR-029–031 | M2 (builder, Samples, Table) → M3 (charts, compare) | |
| FR-032 | M4 | |
| QR-001–003 | M1 | QR-003 verified (crash 10/10); QR-001/002 CI-only |
| QR-004–006, 008 | M3 | |
| QR-007 | M2 | |

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

| # | Item | Why | Done when |
|---|---|---|---|
| R-7 | **Pinned versions for readers**: each query holds a reference to the segment set it started on; a compacted-away segment is deleted only when no reader holds it (replaces the grace-period GC in design-notes §7) | A long query never loses a file mid-scan | Test: a scan runs across a compaction swap and returns the same rows |
| R-8 | **Write slowdown**: when memtable size, unflushed WAL or T0 file count passes a limit, ingest returns `429` + `Retry-After` (the SDK retries) | Memory stays bounded during bursts | Test: a burst past the limit gets 429s, no event lost, memory under the cap |
| R-9 | **Bloom filter on keys**: Parquet bloom filters on `trace_id` and `run_id` in core files | Trace-tree and single-run lookups skip most segments | Bench: segments opened per lookup drops vs. without |
| R-10 | **Two memtables**: one mutable, one frozen while it flushes; writes go to the new mutable one | Ingest never pauses during a flush | Test: ingest latency stays flat while a flush runs |

Not adopted: compaction I/O rate limit, payload-file GC (BlobDB-style), checkpoints, merge operator, FIFO/TTL compaction.

### Not done in M3 (deferred 2026-11-07)

Work M3 planned but didn't finish or couldn't verify here. Each item says what would close it.

| # | Item | Why not done | Done when |
|---|---|---|---|
| D3-1 | **Full 2M-run bench on a quiet machine** (T109) | A clean 1–2 h run on a dedicated machine isn't practical here; the busy-machine run missed run + payload, filtered list, text search p99 and load (before unit 12a's fixes) | Finish the parked branch `wip/m3-bench-three-rows` (three text-search rows; idle row measured on the server and reported "waived (M4)" instead of failing `--check` — without it CI's full bench always fails); then `tracedb bench --runs 2000000 --check` passes every non-waived row on a quiet machine or the nightly CI job, table recorded in M3 research |
| D3-2 | **Idle memory ≤ 200 MB** (SC-05, T087) | Waived for M3 (D-028): 234–246 MB at 0.5M runs, ~0.6 GB projected at 2M, held by merged copies of late-updated runs and per-file state | M4 compaction merges late updates; idle test green at 2M |
| D3-3 | **Cloud CI job's first real run** (T122), incl. M2's | No Docker, MinIO, Postgres or git remote on the build machine | The `cloud` job passes on a remote CI |
| D3-4 | **UI checked in a real browser** (M1, M2, M3 views) | No headless browser in the sandbox; covered by component tests and API smoke tests only | Someone clicks through quickstart step 2 and the Stats/charts/Compare views |
| D3-5 | **No-outbound check under strace** | Needs Linux; built on macOS | `scripts/no-outbound.sh` passes on Linux CI |
| D3-6 | Chart metric and chosen group kept in the URL | Not in the URL contract | A shared link reopens the same chart metric and group |
| D3-7 | Aggregate over a search stops early / cancels | The whole search runs before any estimate; closing the stream doesn't stop it | Estimate within 1 s with a search filter; cancel frees the scan |

## History

- 2026-11-05: product spec, primer, design notes, constitution v1.0.0, plan + Phase 1 artifacts written; the original SPEC.md and REQUIREMENTS.md were folded in and deleted.
- 2026-11-05: M1 implemented (PR1–PR7). Found by recording real SDK traffic: `/info` must send every `batch_ingest_config` key (the SDK reads them without defaults), and an invalid `LANGSMITH_AGENT_ENVIRONMENT` makes the SDK skip tracing silently.
