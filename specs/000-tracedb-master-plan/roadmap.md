# tracedb master roadmap (plan of record)

> This is the master plan and progress tracker for tracedb. Every other doc points here for status and decisions.
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
- 2026-11-05: **Phase details live in the phase folder.** Any new change to M0 goes into `specs/001-m0-see-agent-trace-live/`; this roadmap only links to it and tracks status.
- 2026-11-05: **LangSmith JSON ingest is in M0** (`/info`, `/runs/batch`, `POST/PATCH /runs`), so the M0 demo uses a real agent. Multipart stays in M2.
- 2026-11-05: **Merge rule**: the latest value wins for every field; nothing appends (research R1, because the SDK re-sends full values).
- 2026-11-05: **Constitution v1.1.0**: SmithDB is the target reached in stages; "deliberately staged" is an allowed reason (Principle II (c)). No Simplicity principle; simplicity stays a standing preference.

**STATE as of 2026-11-05** (verified by `git log` and listing `specs/`): the master plan is committed on `master`
(`5e923fc`). M0 is on branch `001-m0-see-agent-trace-live` (worktree `../rust-001`), with spec, design, plan, research,
contracts and quickstart drafted and not committed. No code yet.

**Blocked on**: nothing.

**NEXT**: 1) User reviews M0 [design.md](../001-m0-see-agent-trace-live/design.md) (3 open questions in §11). 2) Commit the M0 docs. 3) `/speckit-tasks` for M0.

## Progress

| Phase | Spec | Status | Tasks done | Exit check |
|---|---|---|---|---|
| M0 See agent trace live | [`001-m0-see-agent-trace-live`](../001-m0-see-agent-trace-live/spec.md) | spec + design drafted, in review | — | not run |
| M1 Durable storage + query builder | — | not started | — | — |
| M2 Core demo | — | not started | — | — |
| M3 Follow-through | — | not started | — | — |
| M4 Zero instrumentation + agents | — | not started | — | — |

"Tasks done" counts `[X]` in each phase's `tasks.md`. A phase is **done** only when its exit check passes, not when its PRs merge.

## Phases

### M0: See agent trace live

> Details of record: [`specs/001-m0-see-agent-trace-live/`](../001-m0-see-agent-trace-live/spec.md). The summary below is the original scope; the phase folder wins if they differ.

**Goal and why**: prove the whole path (agent → ingest → durable log → query → browser) works end to end, thinly.
It **unblocks** every later phase: M1 puts storage under the same API, and M2 hardens the same ingest. It also gives
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
| WAL with group commit + memtable + replay | Direct: the kill -9 check. Indirect: M1 flushes this memtable to disk |
| Ingest: `/v1/events`, `/info` (advertises no multipart), `/runs/batch`, `POST/PATCH /runs` | Direct: an unmodified agent traces in (the headline demo) |
| Query: trace tree, single run, root-runs list with cursor, live tail | Direct: the runs list and trace view have data. Indirect: M1 swaps in DataFusion behind the same API |
| UI: runs list (live), trace tree + waterfall, run panel, empty state, shareable URLs | Direct: the demo is in the browser |
| `tracedb seed` (deep trees, late ends, big payloads) | Direct: the 250-run demo. Indirect: realistic data for UI work and M1 scale tests |
| Crash test, no-outbound check, SDK contract test in CI | Durable: keeps the end state true after M0 |

**Delivers**: FR-001 (JSON path), FR-002–007, FR-012 (basic), FR-022 (list + trace), FR-025, FR-027 (partial); QR-001, QR-002, QR-003.

### M1: Durable storage + query builder

**Goal and why**: hold real volumes on disk and let people find things by clicking. This **unblocks** M2's speed and
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

### M2: Core demo

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

### M3: Follow-through

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
| Compaction tiers + atomic swap | Direct: late-end correctness after compaction. Indirect: keeps M2 speeds as data grows |
| Threads view; export; feedback write | Direct: steps 10, 11 and feedback filters |
| Find-what-stands-out | Direct: Story 12 |

**Delivers**: FR-016, FR-017, FR-020, FR-032.

### M4: Zero instrumentation + agents

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
| FR-001 | M0 (JSON), M2 (multipart) | |
| FR-002–007 | M0 | |
| FR-008–010, 012, 013 | M1 | |
| FR-011, 014, 015 | M2 | |
| FR-016, 017, 020 | M3 | |
| FR-021, 028 | M4 | |
| FR-022–027 | M0 → M2 (grows each phase) | |
| FR-029–031 | M1 (builder, Samples, Table) → M2 (charts, compare) | |
| FR-032 | M3 | |
| QR-001–003 | M0 | |
| QR-004–006, 008 | M2 | |
| QR-007 | M1 | |

Mark ✅ only after the phase's exit check passes.

## Backlog: SmithDB reconciliation (found 2026-11-05)

From checking spec 000 against the SmithDB post, the LangSmith self-hosted docs, and `langsmith` SDK 0.8.18 vs 0.14.17.
**Local** = does the laptop version need it.

| # | Item | Local? | When | Done when |
|---|---|---|---|---|
| R-1 | Re-pin the SDK from 0.8.18 to 0.14.x (Python; JS 0.11.x). Accept the new run `address` field (agent id + environment) and store it as metadata, don't reject it | **Yes**: unmodified agents run the latest SDK | M0 (`specs/001-…`) | Contract fixtures recorded with 0.14.x; a run with `address` is accepted |
| R-2 | Use LangSmith's current `field:value` filter syntax (`-`, `~`, `*`, `>`, `[a TO b]`, AND/OR/NOT, `a.b` JSON paths; scopes thread / root / run / any) as the query builder's text form, replacing the `eq()/and()` grammar in design-notes §5.2 | **Yes**: it's what LangSmith users type now | Decide before M1 | design-notes §5.2 rewritten; M1 builder round-trips panel ↔ text |
| R-3 | Fix design-notes wording: §6 "fixed-size chunks" → "terms in row groups with min/max term zones; postings and positions chunked apart from the term dictionary"; add SmithDB ingest numbers (P50 630 ms / P99 1.47 s) to §2 | Doc only | Anytime | Text matches the post |
| R-4 | Pin `object_store` to the 0.13 line (what `vortex-datafusion` 0.91 needs) and update the Vortex reason in plan.md / research R4 to "pre-1.0, not benchmarked by us" (it now supports DataFusion 55) | Cheap, keeps the Vortex swap open | M1, when crates are added | `Cargo.toml` pin + updated reason |
| R-5 | If we add LangSmith *read* compatibility, copy the v2 API (`/api/v2/runs/query`, `/api/v2/threads/query`, project UUIDs, no child runs in run queries); v1 is removed on Cloud 2027-01-31 | No: compat is ingest-only (spec.md Assumptions) | Deprioritized | — |
| R-6 | Cloud path: split Compaction into a single scheduler plus scalable workers, and the cluster manager into its own service (self-hosted SmithDB runs 5 services) | No: one process locally | Cloud | design-notes §2.1 cloud column updated |

## History

- 2026-11-05: product spec, primer, design notes, constitution v1.0.0, plan + Phase 1 artifacts written; the original SPEC.md and REQUIREMENTS.md were folded in and deleted.
