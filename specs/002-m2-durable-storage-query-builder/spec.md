# Feature Specification: M2 — Durable storage + query builder

**Feature Branch**: `002-m2-durable-storage-query-builder`

**Created**: 2026-11-06

**Status**: Built (M2 closed at `c7595c6`). Where the code differs: [decision log](../000-smithling-master-plan/decisions.md) D-012–D-024 (while building M2), D-025–D-029 (M3)

**Input**: User description: "M2 'Durable storage + query builder' for smithling … scope from roadmap M2 … fold in backlog R-2 and
R-7–R-10, plus three items from the M1 code review (tail loop into the query service, serve UI files from disk, UI built
as its own CI step) and the M1 rough edge (a retried rejected item reads as accepted). Write it descriptively: goal + why,
end state as 'As a <who>, I can <what>' with the general capability first and a labeled Example, work tied to end state,
plus the lifecycle of one run through M2. Reference master requirement IDs instead of copying."

**Master**: [specs/000-smithling-master-plan](../000-smithling-master-plan/spec.md) (requirements of record) ·
[roadmap](../000-smithling-master-plan/roadmap.md) (progress). This folder holds the M2 details of record.

**Builds on**: [M1 — See agent trace live](../001-m1-see-agent-trace-live/spec.md). Everything M1 promises stays true.

**Delivers from master**: FR-008 (without feedback score), FR-009, FR-010, FR-012, FR-013, FR-026, FR-029 (where, order,
limit; group by / metrics / comparison arrive in M3), FR-030, FR-031 (Samples and Table views; Time series and
Distribution arrive in M3); QR-007. Backlog: R-2 (optional `field:value` form), R-7, R-8, R-9, R-10.

---

## Goal and why

Hold real volumes on disk, and let people find the runs that matter by clicking.

- **It removes M1's ceiling.** M1 keeps every run in memory and refuses new runs at about 500k. Real use (a nightly
  200-task eval batch, every night) passes that within weeks. M2 keeps data on disk, so the limit becomes disk space.
- **It answers the first half of morning triage** (master scenario steps 4, 5 and 8): *what failed* and *what failed
  quietly*. Without filters, a store is just a pile of traces.
- **It unblocks M3.** Text search, aggregates and the speed work all run over the same disk files and the same query
  shape that M2 introduces.
- **It moves us closer to SmithDB.** M2 adds the other half of SmithDB's design: immutable files on (local or cloud)
  storage, a metadata catalog of those files, and queries that combine fresh in-memory data with files on disk.

## End state

*Capability first; the Example under it is the scenario we use to prove it.*

- As an **agent developer**, I can narrow all my runs to the ones I care about by time, tags, status, latency, run
  type, name, model, and any metadata, input or output field, combined with AND / OR / NOT. I build the query with a
  point-and-click panel, with no syntax to learn.
  - *Example*: last 12 h, tag `batch=2026-11-05-a`, root runs, status error → exactly **14** runs.
- As an agent developer, I can find runs by what happened *anywhere in their trace*, not just in the run itself.
  - *Example*: root runs with status ok where any run in the same trace errored (quiet failures) → exactly **23** runs.
- As an agent developer, I can see every field in my data, including metadata keys nobody declared, with its type
  and most common values and counts. One click on a value filters by it.
  - *Example*: the field browser shows `prompt_version: v6 (928), v7 (912)`; clicking `v7` adds that filter.
- As an agent developer, I can switch between the panel and a text form of the same query. Each updates the other,
  and a mistake in the text is pointed at, not silently ignored.
  - *Example*: `and(eq(status, "error"), has(tags, "batch=2026-11-05-a"))` fills the panel with two chips; a typo in a
    field name is underlined with "unknown field `staus`".
- As an agent developer, I can see the matching runs either as a list of samples or as a table with columns I choose,
  and page through them newest first with no gaps or repeats, even while new runs arrive.
- As an agent developer, I can keep a filtered list open and watch new runs that match it appear live.
  - *Example*: a live view of `status = error AND name = fetch_url` during the next eval batch.
- As an agent developer, I can share any view as a link that reopens exactly the same query and view.
- As an agent developer, I can keep smithling running for weeks and restart it at any time without losing data, at
  laptop scale (millions of runs).
  - *Example*: 2M runs, restart, and the last query returns the same rows within 10 s of starting.
- As an agent developer, I can keep uploading during a burst and never see my agent fail. When smithling is busy it
  asks the agent to slow down, and the agent's SDK retries on its own.
- As a **test user**, I can generate the overnight-eval scenario and run its first checks, which reproduce the
  example numbers above.
  - *Example*: one command prints `14 failing roots ✓  23 quiet failures ✓  prompt_version top values ✓`.
- As a **maintainer**, I can trust queries return the same results on local disk and on cloud storage: automated
  checks run the scenario on both.
- As a **maintainer**, I can work on the UI without rebuilding the server, and build the server without the UI
  toolchain.
- As a **new reader** of the code, I can follow one run through the M2 code (flush, restart, merge-on-read, query,
  live views, panel ⇄ text) in a starter guide with line-level links, like M1's.

## Lifecycle of one run in M2

The `fetch_url` step from the master scenario again. Stages 1–5 work as in M1; the new parts start at stage 6. The last
column names the M2 work that makes each stage happen.

| # | When | What happens | M2 work |
|---|---|---|---|
| 1 | 02:14:07 | The step's **start** arrives, is recorded durably, and is held in memory (as in M1). | (M1) |
| 2 | +0 s | Any open screen whose query matches it, not just the runs list, shows it within 1 s. | Live views for any query |
| 3 | 02:14:38 | Its **update** arrives (error, outputs with `status_code: 429`) and is merged in memory. | (M1) |
| 4 | burst | Thousands of other runs arrive at once. smithling tells some uploads "busy, retry shortly"; the SDK retries and nothing is lost. | Write slowdown (R-8) |
| 5 | ~02:20 | The in-memory batch fills. It is frozen and written to disk as immutable files: one with the small fields used for filtering, one with the payloads. New uploads go to a fresh in-memory batch, so they don't pause. | Write to disk; two in-memory batches (R-10) |
| 6 | flush done | The files are registered in the metadata catalog. Only then is the matching part of the durable log trimmed. A crash at any point leaves either the log or the files, never neither. | Catalog; safe log trimming |
| 7 | 02:31 | A late update for the same step arrives after its files were written. It lands in memory, and queries merge it with the file version (latest value wins), so the step shows the newer state. | Merge on read |
| 8 | ongoing | Field names and values seen in the step (`metadata.prompt_version = v7`, `outputs.status_code = 429`) are counted in the field catalog. | Field catalog |
| 9 | 08:00 | The developer filters to quiet failures. The query skips files whose time range or value summaries can't match, reads only the filter columns, and finds the step's root among the **23**. | Query engine with skipping; tree-aware filters |
| 10 | 08:01 | They open the trace. smithling finds the files holding it quickly, without opening every file. | Key lookup summaries (R-9) |
| 11 | 08:02 | A long query is still running when new files appear. It keeps reading the file set it started with, and no file is deleted under it. | Pinned file sets (R-7) |
| 12 | anytime | Restart: smithling reads the catalog, replays only the untrimmed log, and is ready in seconds. | Fast restart |
| 13 | every change | Automated checks run the scenario checks on local disk and on cloud storage. | Demo checks; cloud CI |

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Find the failures with the query builder (Priority: P1)

The developer narrows millions of runs to the handful that matter, by clicking: time, tags, status, latency, type,
name, model, and any JSON field, plus conditions on the whole trace.

**Why this priority**: this is the morning triage (master steps 4, 5 and 8) and the main reason for M2.

**Independent Test**: load the overnight-eval scenario, build each query in the panel without typing syntax, and
confirm exactly 14 failing roots and exactly 23 quiet failures, newest first, across several pages.

**Acceptance Scenarios**: master User Story 3, scenarios 1–7 and 10–11. In addition:

1. **Given** the scenario data, **When** the developer combines conditions with OR and NOT (for example, status error OR latency over 30 s, NOT name `plan`), **Then** the result matches the same condition checked run by run.
2. **Given** a query, **When** the developer switches between the panel and the text form, **Then** both describe the same query and return the same runs.
3. **Given** a filter on a field that only some runs have, **When** it is run, **Then** runs without the field don't match, and an "exists" condition finds exactly the runs that have it.
4. **Given** a numeric field stored as numbers in some runs and text in others, **When** it is compared (for example `outputs.status_code >= 400`), **Then** only the numeric values are compared, and the developer is told some values were not numbers.

---

### User Story 2 - Explore fields and filter by clicking values (Priority: P1)

The developer doesn't need to know what fields exist. The field browser shows them, with top values and counts, and
any value seen anywhere in the UI can be clicked into the filter.

**Why this priority**: it makes triage possible without reading docs or knowing the data's shape (master SC-008,
SC-010).

**Independent Test**: on the scenario data, open the field browser, find `prompt_version` with `v6 (928)` and
`v7 (912)`, click `v7`, then click a tag and a status in the results, and confirm the filter has three chips and the
results match.

**Acceptance Scenarios**: master User Story 3, scenarios 6, 8 and 9. In addition:

1. **Given** a metadata key that first appears in today's runs, **When** the field browser is opened, **Then** the key is listed with its type and counts.
2. **Given** the developer types part of a field name or value, **When** suggestions appear, **Then** they show counts and appear within 500 ms at 2M runs.
3. **Given** the Table view, **When** the developer adds a column for any field, **Then** it shows that field's value for each run, and clicking a cell value adds it to the filter.

---

### User Story 3 - Keep all data on disk, restart fast (Priority: P1)

The developer leaves smithling running for weeks. Data goes to disk in the background, restarts are fast, and nothing
confirmed is ever lost.

**Why this priority**: M1's memory ceiling makes smithling unusable after a few weeks of real use.

**Independent Test**: load 2M runs (more than M1 could hold), kill smithling at random points during upload and during
writes to disk, restart, and compare the stored runs and query results with every confirmed upload, over 10 rounds.

**Acceptance Scenarios**:

1. **Given** more runs than fit in the memory budget, **When** uploads continue, **Then** they are accepted and older data moves to disk; no upload is refused for lack of memory.
2. **Given** an abrupt kill during a write to disk, **When** smithling restarts, **Then** every confirmed event is present exactly once.
3. **Given** a step written to disk, **When** a late update for it arrives, **Then** queries and the trace view show the merged, newest state.
4. **Given** 2M stored runs, **When** smithling restarts, **Then** it accepts uploads and answers queries within 10 s.
5. **Given** a burst of uploads past what smithling can absorb, **When** it is busy, **Then** it answers "busy, retry shortly", the SDK retries, no event is lost, and memory stays under the configured limit.
6. **Given** a query running while new files are written, **When** it finishes, **Then** its results are complete and consistent with the moment it started.

---

### User Story 4 - Live views for any query (Priority: P2)

Any filtered list can stay live: new and updated runs that match the query appear without a refresh.

**Why this priority**: M1 has live views only for the plain runs list and one trace. Watching a specific failure
during a run is common, but triage works without it.

**Independent Test**: open a live view for `status = error AND name = fetch_url`, upload a mix of runs, and confirm
only matching runs appear, within 1 s.

**Acceptance Scenarios**:

1. **Given** a live view with a filter, **When** a run that matches arrives, **Then** it appears within 1 s; **When** a run that doesn't match arrives, **Then** nothing changes.
2. **Given** a run that stops matching after an update (for example, it was running and is now ok), **When** the update arrives, **Then** the view shows its new state, marked as no longer matching, rather than silently keeping the old one.
3. **Given** a tree-aware filter (quiet failures), **When** a child run errors after its root finished ok, **Then** the root appears in the live view.

---

### User Story 5 - Reproduce the scenario numbers with one command (Priority: P2)

A test user generates the overnight-eval scenario and runs its first checks.

**Why this priority**: it proves the end state numbers, and keeps them true from M2 on.

**Independent Test**: on an empty data folder, run the demo command and confirm it reports the M2 checks as passing.

**Acceptance Scenarios**:

1. **Given** an empty smithling, **When** the demo command runs, **Then** it generates the scenario (200 tasks, planted failures, late-finishing roots) and reports pass/fail for: 14 failing roots, 23 quiet failures, and `prompt_version` top values.
2. **Given** a check fails, **When** the demo command ends, **Then** it names the failing check with expected and actual values, and exits with an error.
3. **Given** the same seed, **When** the demo runs twice, **Then** the generated data is identical.

---

### User Story 6 - Same results on cloud storage (Priority: P2)

A maintainer runs the same smithling against cloud-style object storage and a hosted metadata database, chosen by
configuration only, and gets the same results.

**Why this priority**: it is a constitution principle (V, cloud-portable storage). Proving it in M2, when the storage
layer is first written, is far cheaper than retrofitting it later.

**Independent Test**: run the demo checks and the crash test with local storage, then with a local stand-in for cloud
object storage and a hosted-style metadata database, and compare.

**Acceptance Scenarios**:

1. **Given** a configuration pointing at object storage and a hosted metadata database, **When** smithling starts, **Then** it needs no code change and no other setting.
2. **Given** either configuration, **When** the demo checks run, **Then** they produce the same numbers.

---

### User Story 7 - Maintainer workflow and M1 follow-ups (Priority: P3)

Small changes found while reviewing M1, which make smithling easier to work on and more honest in its replies.

**Why this priority**: none is user-visible on its own, but each removes friction or a misleading answer.

**Independent Test**: each item has its own check below.

**Acceptance Scenarios**:

1. **Given** an item rejected for a conflict (for example a changed trace id), **When** the client retries the identical item, **Then** it is reported as rejected again with the same reason, not as accepted.
2. **Given** a developer working on the UI, **When** they point a running smithling at a folder of UI files, **Then** it serves those files instead of the built-in ones, with no server rebuild.
3. **Given** a machine without the UI toolchain, **When** the server is built, **Then** the build succeeds using a prebuilt UI; the UI is built and checked as its own automated step.
4. **Given** the live-view logic, **When** a new kind of view needs live updates, **Then** it reuses the same query service as any other read, not code in the web layer.

---

### Edge Cases

- **Late update after a step is on disk**: queries, the trace view and live views show the merged state, whether the late part is in memory or also on disk.
- **Same run in several files** (several updates flushed at different times): every read shows one merged run, never two.
- **Crash between writing files and registering them**: unregistered files are ignored and cleaned up; the log still holds those events.
- **Crash after registering files but before trimming the log**: replaying the log doesn't create duplicates.
- **Disk full during a write to disk**: uploads are refused with a clear reason; nothing confirmed is lost; the write is retried once space is free.
- **A field with mixed types** across runs (number in one, text in another): it is listed once with each type and its count; comparisons use matching types only.
- **Very many distinct metadata keys** (for example one key per user id): the field catalog keeps the first 10,000 keys per project and says the list is incomplete.
- **Very many distinct values** for one field: the catalog keeps top values and counts, not every value.
- **Text query with a mistake**: the message points at the position and says what was expected; no results are shown for a broken query.
- **Saved link from M1** (project and status only): it still opens, translated to the M2 query.
- **Tree-aware filter on a trace still running**: the result reflects the runs received so far, and live views update as more arrive.
- **Data folder from M1**: M2 starts on it, replays the M1 log, and writes it to disk files; no manual migration.

## Requirements *(mandatory)*

Each requirement names the master requirement or backlog item it delivers. The master text is not repeated here.

### Functional Requirements

**Find runs**

- **M2-FR-01** (→ FR-008): Users MUST be able to filter runs by time range, tags, status, latency, run type, name and model, combined with AND / OR / NOT. Feedback score is excluded until feedback exists (M4).
- **M2-FR-02** (→ FR-009): Users MUST be able to filter on any path in metadata, inputs and outputs with equality, comparison and existence.
- **M2-FR-03** (→ FR-010): Users MUST be able to combine, in one query, conditions on the run, on the root run of its trace, and on any run in the same trace.
- **M2-FR-04** (→ FR-012): Results MUST come newest first by default, paged with a cursor that gives no gaps or repeats while data arrives, and MUST let the user choose another order on time or latency.
- **M2-FR-05** (→ FR-013): A query with a mistake MUST be rejected with a message that points at the offending part and says what was expected.
- **M2-FR-06** (→ R-2): The text form MUST be the LangSmith filter grammar (`eq`, `and`, `has`, …). A shorter `field:value` form in the search bar MAY be added if LangSmith's UI docs confirm it; it is not required for M2's exit check.

**Query builder and views**

- **M2-FR-07** (→ FR-029, partial): The UI MUST offer a query panel (project, time range, where-conditions as chips, order, limit) that stays in step both ways with the text form. Group by, metrics and comparison arrive in M3.
- **M2-FR-08** (→ FR-026): Users MUST be able to add a filter by clicking any status, tag, name, metadata value or table cell, and negate, disable or remove any chip without retyping.
- **M2-FR-09** (→ FR-030): System MUST keep a field catalog of every field, including discovered metadata, input and output keys, with type, count and top values; it MUST power the field browser and suggestions.
- **M2-FR-10** (→ FR-031, partial): Every query MUST be viewable as Samples and as a Table with user-chosen columns, without rebuilding it.
- **M2-FR-11** (→ FR-022, FR-027): Every query and view MUST have a shareable link; M1 links MUST keep working.
- **M2-FR-12** (→ FR-003): Any query view MUST be able to stay live, showing matching new and updated runs within 1 s.

**Storage**

- **M2-FR-13** (→ QR-003, master design): System MUST move data from memory to immutable files on disk in the background, without pausing uploads (→ R-10). Confirmed events MUST survive a crash at any point of that move.
- **M2-FR-14**: System MUST trim the durable log only after the data it holds is safely in registered files.
- **M2-FR-15** (→ FR-002): Every read MUST show one merged run, whether its events are in memory, in files, or both.
- **M2-FR-16** (→ R-7): A query MUST read a fixed set of files from start to finish; no file it uses is removed before it ends.
- **M2-FR-17** (→ R-9): Looking up one trace or one run MUST not need to open every file.
- **M2-FR-18** (→ R-8): When uploads come faster than they can be stored, system MUST ask clients to retry shortly instead of refusing data or exceeding its memory limit. The M1 "memory full" refusal goes away.
- **M2-FR-19** (→ QR-007): The same program MUST store files on local disk or cloud object storage, and its catalog in a local or hosted database, chosen by configuration only.
- **M2-FR-20**: A data folder written by M1 MUST open in M2 with no manual step.

**Checks and maintainer workflow**

- **M2-FR-21** (→ FR-024, partial): System MUST provide a demo command that generates the overnight-eval scenario and checks the 14 failing roots, 23 quiet failures and `prompt_version` top values.
- **M2-FR-22** (constitution VI): Automated checks MUST run the demo checks and the crash test (now including crashes during writes to disk) on local storage and on cloud-style storage.
- **M2-FR-23** (M1 review): Retrying an item that was rejected MUST report the same rejection again.
- **M2-FR-24** (M1 review): A running smithling MUST be able to serve UI files from a chosen folder instead of the built-in copy.
- **M2-FR-25** (M1 review): The server MUST build without the UI toolchain; the UI MUST be built and tested as its own automated step.
- **M2-FR-26** (M1 review): Live views MUST be served by the query service through the same filter as any other query, not by logic in the web layer.
- **M2-FR-27** (2026-11-06 decision): M2 MUST end with a code starter guide (`starter-guide.md`) that maps each end-state flow to the real functions, with links and line numbers.

### Key Entities

- **Run, Trace, Project**: as in M1.
- **Query**: one shape for every read: project, time range, conditions (on the run, its root, or any run in its trace), order, limit, and view (Samples or Table with columns). Has a text form and a link form.
- **Data file**: an immutable file holding a set of runs, with summaries of what it contains (time range, value ranges, key lookups). The small filter fields and the large payloads are kept in separate files.
- **Catalog**: the record of which data files exist and which are current; reads take a fixed snapshot of it.
- **Field**: one entry in the field catalog: path, type(s), number of runs that have it, top values with counts.
- **Live view**: a query kept open, receiving matching changes.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-01**: On the scenario data, the query panel returns exactly 14 failing roots and exactly 23 quiet failures, built without typing any filter syntax (master SC-010 for steps 4, 5 and 8).
- **SC-02**: A developer new to smithling finds the 23 quiet failures on the scenario data in under 5 minutes, using only the UI.
- **SC-03**: At 2M runs on the reference laptop, a filtered list returns its first page with median ≤ 500 ms; a trace tree and a single run open with median ≤ 200 ms; field and value suggestions appear with median ≤ 500 ms. (M3 tightens these to the master QR-004 and QR-008 targets.)
- **SC-04**: smithling holds 2M runs with memory staying under its configured limit, and data on disk takes no more than the same data as raw JSON.
- **SC-05**: In 10 of 10 abrupt-kill rounds, including kills during writes to disk, zero confirmed events are missing or duplicated after restart.
- **SC-06**: With 2M stored runs, smithling is ready within 10 s of starting.
- **SC-07**: Upload throughput stays at or above M1's 5,000 events/s while data is being written to disk, and no upload waits more than 1 s because of it.
- **SC-08**: The demo checks give identical numbers on local storage and on cloud-style storage.
- **SC-09**: Results for every scenario query match a run-by-run check of the same condition on 100% of runs, including runs whose late updates arrived after they were written to disk.
- **SC-10**: All M1 success criteria still pass.

## Assumptions

- **Speed targets are M2-sized** (SC-03, SC-07). The master targets (QR-004, QR-005, QR-008) apply from M3, which owns the speed work.
- **Scale**: "laptop scale" means about 2M runs / 5 GB, the master reference size.
- **Query grammar**: the text form is LangSmith's filter grammar, because SDK 0.14.17 still documents it (backlog R-2). It covers the master's AND / OR / NOT, comparisons, existence and tree conditions.
- **"Any run in the trace"** covers the whole trace, not a subtree (master assumption).
- **Feedback score** filters wait for feedback, in M4.
- **Text search, group by, metrics, charts and comparison** are M3. M2's query shape leaves room for them so the panel and links don't change.
- **Background reorganization of files** (compaction) is M4. M2 writes files and never merges them, so file count grows with data; M2's speed targets are measured with that.
- **Cloud check** uses local stand-ins for cloud object storage and a hosted database in automated checks; no real cloud account is needed, and no outbound connection is made outside that check's own containers.
- **The "busy, retry" answer** relies on the SDK retrying, which the pinned SDK versions do (M1 recorded traffic).
- **Single user**, localhost only, no login (unchanged).
- **Out of scope for M2**: text search, aggregates and charts, comparison, threads, export, feedback, compaction, multipart upload, delete and retention, Claude Code import, agent tools.
