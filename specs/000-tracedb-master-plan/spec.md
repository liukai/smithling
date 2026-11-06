# Master Spec: tracedb — a local trace store and viewer for AI agents

> **MASTER PLAN.** This folder (000) is the tracedb master: requirements of record, architecture and roadmap. It has
> no code tasks. Each phase is its own spec folder (`specs/001-…`, `specs/002-…`) that delivers requirement IDs from
> here. Phase details live in the phase folder; this folder links to it. Progress: [roadmap.md](roadmap.md).

**Feature Branch**: `000-tracedb-master-plan`

**Created**: 2026-11-05

**Status**: Master (requirements of record)

**Input**: User description: "use SPEC.md as the starting point, also requirements.md; also want to make requirements very easy to understand problem, almost like tutorial on why this matters and how things should look like as the end state"

**Companions**:
- [primer.md](primer.md): a tutorial on why this matters (what traces are, why existing tools fall short) and a walkthrough of the finished product. Read it first if you're new to the problem.
- [design-notes.md](design-notes.md): the draft design (storage layout, engine, milestones).
- [roadmap.md](roadmap.md): master plan and progress (plan of record).

**In one paragraph**: tracedb is one local program that captures traces from an unchanged AI agent and lets a developer browse, search, filter and aggregate them in a browser. All data stays in one folder on their laptop. The driving scenario is triaging an overnight 200-task eval batch: what failed, what failed quietly, is it a pattern, how bad is it, and did the fix work.

---

## User Scenarios & Testing *(mandatory)*

**Priorities**: P1 = needed for the core demo (scenario steps 1–9). P2 = the rest of the scenario. Later items are out of scope (see Assumptions).

**UI first**: the browser UI is how people use tracedb, not a layer added once the storage foundation is done. Every
story ships with its browser view in the same slice, and every Independent Test below is run in the browser. The very
first slice (Story 1 + Story 2) is a working runs list and trace view, even if the data behind it is small and in memory.
The CLI and API are equivalents for scripting; no capability is available only through them.

### User Story 1 - Capture traces from an unchanged agent and see them live (Priority: P1)

A developer starts tracedb, points their existing agent at it with one environment variable, and watches runs show up
in the browser as they happen. Runs that take a long time show as running and update when they finish.

**Why this priority**: Nothing else works without data. "No code changes" is what makes people try it at all.

**Independent Test**: Start tracedb on an empty data folder, run an unmodified LangChain/LangGraph example agent with
the endpoint variable set, and confirm its trace appears in the UI within 1 second of each step completing.

**Acceptance Scenarios**:

1. **Given** tracedb is running and an agent built on the LangSmith SDK has `LANGSMITH_ENDPOINT` pointed at it, **When** the agent runs a task, **Then** the full trace appears without any change to the agent's code.
2. **Given** a run whose start arrived 40 minutes ago, **When** its end arrives, **Then** the run's status, end time and outputs update in the runs list, the trace view and query results.
3. **Given** a run that has received several updates, **When** it is viewed, **Then** it shows a single merged state where later updates override earlier ones.
4. **Given** the runs list is open, **When** new runs are ingested, **Then** they appear in the list (live tail) without a manual refresh.
5. **Given** tracedb has acknowledged events, **When** the process is killed abruptly and restarted, **Then** every acknowledged event is still present.

---

### User Story 2 - Open a trace and inspect any step (Priority: P1)

The developer opens one trace and sees its whole tree and a timing waterfall at once. They click any step to see its
full inputs and outputs.

**Why this priority**: "What happened in this one?" is the most common debugging question.

**Independent Test**: Ingest a 250-run trace with 40 KB payloads. Open it and confirm the tree renders in
parent-before-child order before any payload loads, then open one step and see its full payload.

**Acceptance Scenarios**:

1. **Given** a stored trace with 250 runs, **When** the developer opens it, **Then** the full tree (names, types, status, timing, tokens) appears in depth-first order without loading any inputs or outputs.
2. **Given** an open trace, **When** the developer selects a run, **Then** that run's full inputs and outputs are shown.
3. **Given** a trace whose root is still running, **When** it is opened, **Then** the finished child runs appear and the root shows as running.

---

### User Story 3 - Find the failures: filtering, tree-aware and JSON filters (Priority: P1)

The developer narrows millions of runs down to the handful that matter, using time, tags, status, latency, run type,
name, feedback score, JSON fields, and conditions on the whole tree. They do it in a Scuba/Honeycomb-style **query
builder**: a panel of controls (time, where, group by, show, order) plus a field browser, refined by clicking results
rather than typing syntax.

**Why this priority**: This is the morning triage (steps 4, 5, 8). Without it, a store is just a pile of traces.

**Independent Test**: Load the overnight-eval scenario data. In the runs list, use the filter bar (with chips and
autocomplete for fields and values) to get exactly 14 failing root runs, then exactly 23 quiet failures, newest first, scrolling across pages.

**Acceptance Scenarios**:

1. **Given** the scenario data, **When** the developer filters to the last 12 h, tag `batch=2026-11-05-a`, root runs only, status error, **Then** exactly 14 runs are returned.
2. **Given** the same data, **When** they filter for root runs with status ok where *any* run in the same trace has status error, **Then** exactly 23 runs are returned.
3. **Given** runs with user-defined metadata and structured tool outputs, **When** they filter on `metadata.prompt_version = "v7"` and `outputs.status_code = 429`, **Then** only runs matching both are returned.
4. **Given** more results than fit on one page, **When** they page forward, **Then** results continue newest first with no duplicates or gaps, even while new runs are arriving.
5. **Given** a filter expression with an error, **When** it is submitted, **Then** the developer sees a message that points at the problem, not an empty result.
6. **Given** a value shown anywhere in the UI (a tag, a status, a run name, a metadata value), **When** the developer clicks it, **Then** it is added to the filter, so no filter syntax is needed for common triage.
7. **Given** a filtered view, **When** the developer copies the page address, **Then** opening it later restores the same filter and view.
8. **Given** the field browser, **When** the developer opens it, **Then** it lists every field, including metadata keys discovered from the data, with its type and top values with counts (e.g. `prompt_version`: `v7 (912)`, `v6 (928)`).
9. **Given** the developer is adding a filter, **When** they type a field or value, **Then** suggestions with counts appear as they type.
10. **Given** filter chips in the panel, **When** the developer negates, disables or removes one, **Then** results update without retyping anything.
11. **Given** a query built in the panel, **When** the developer views it as text, **Then** they see the equivalent filter expression, and pasting an expression fills the panel the same way.

---

### User Story 4 - Search the text inside inputs and outputs (Priority: P1)

The developer searches for a phrase such as `"429 Too Many Requests"` across all inputs and outputs, and combines it
with the filters from Story 3.

**Why this priority**: Step 7, "is this a pattern?" Errors often live only in payload text.

**Independent Test**: In the runs list search box, search for `"429 Too Many Requests"` and confirm 31 runs come back, all named `fetch_url`, with the matching text highlighted in each result.

**Acceptance Scenarios**:

1. **Given** the scenario data, **When** the developer searches outputs for the exact phrase `"429 Too Many Requests"`, **Then** 31 runs are returned, all `fetch_url`.
2. **Given** a search combined with a tag filter and a tree-aware filter, **When** it runs, **Then** only results that satisfy all conditions are returned.
3. **Given** a phrase that appears in no run, **When** searched, **Then** an empty result comes back within the search response-time target.

---

### User Story 5 - Add it up and compare batches (Priority: P1)

The developer aggregates runs (count, error rate, latency percentiles, tokens, cost, average feedback score) grouped by
name, model, tag or metadata key, and shows two batches side by side.

**Why this priority**: Step 9, "how bad is it?", and step 13, "did my fix work?" Numbers turn a hunch into a decision.

**Independent Test**: In the stats view, group the scenario's batch-a runs by name and confirm the table and charts show
a 14% error rate and a p95 latency of 31 s for `fetch_url`, and a batch total cost of $4.12.

**Acceptance Scenarios**:

1. **Given** batch-a data, **When** grouped by run name, **Then** each group shows count, error rate, p50/p95 latency, total tokens and total cost, and `fetch_url` shows 14% / p95 31 s.
2. **Given** batches a and b, **When** the developer groups by tool name and splits by batch tag, **Then** both batches' figures appear side by side for each tool.
3. **Given** any filter from Stories 3–4, **When** aggregation runs, **Then** it covers exactly the filtered runs.
4. **Given** a grouped result, **When** shown, **Then** it appears as both a table and charts (error rate by group, latency distribution, tokens and cost over time), and clicking a bar or row opens the runs behind it.
5. **Given** any query, **When** the developer switches between the Samples, Table, Time series and Distribution views, **Then** the query is kept and only the presentation changes.
6. **Given** a time series, **When** the developer turns on comparison (another batch, or the same window shifted back), **Then** both appear overlaid on the same axes.

---

### User Story 6 - Rebuild a conversation thread (Priority: P2)

The developer opens a multi-turn task as a thread: all its traces, in time order, shown as a conversation.

**Why this priority**: Step 10, "what did the user see?" This matters for chat agents but isn't needed for the core triage.

**Independent Test**: From a run in the runs list, open its thread and confirm 6 traces appear in time order as chat turns, each linking to its trace view.

**Acceptance Scenarios**:

1. **Given** 6 root runs sharing a thread id, **When** the thread is opened, **Then** all 6 appear in start-time order, each showing its user input and final output.
2. **Given** a thread where one turn is still running, **When** opened, **Then** that turn shows as in progress.

---

### User Story 7 - Export bad runs as a regression dataset (Priority: P2)

The developer exports every run matching a filter (inputs, outputs, metadata) to a file for reuse as an eval dataset.

**Why this priority**: Step 11, "never again." It closes the loop from failure to test.

**Independent Test**: With the 37 bad root runs filtered in the runs list, click Export and confirm the downloaded file has 37 records, each with inputs, outputs and metadata.

**Acceptance Scenarios**:

1. **Given** the 37 failing root runs from Story 3, **When** exported as JSONL, **Then** the file holds 37 lines, each with run id, inputs, outputs and metadata.
2. **Given** a filter matching tens of thousands of runs, **When** exported, **Then** the export finishes without running out of memory, and the record count matches the filter count.

---

### User Story 9 - Attach feedback after the fact (Priority: P2)

The developer (or a script) attaches a score and comment to a run after it finished. Feedback can then be filtered and averaged.

**Why this priority**: The post treats feedback as a first-class filter. Filtering and averaging seeded feedback is P1
(Stories 3, 5); attaching new feedback interactively can come later.

**Independent Test**: In a run's side panel, attach a score of 0 with a comment, then filter by `feedback.score < 1` and confirm the run appears.

**Acceptance Scenarios**:

1. **Given** a finished run, **When** feedback (key, score, optional comment) is attached, **Then** the run shows it and it can be filtered and averaged.

---

### User Story 10 - Import local Claude Code sessions (Priority: P2)

The developer imports their Claude Code session history and uses the same triage tools on it, with no instrumentation.

**Why this priority**: Variant B. It's a second audience with zero setup, but it isn't needed for the main scenario.

**Independent Test**: Import a folder of Claude Code session files, then confirm in the UI that each session shows as a
trace, tool calls as tool runs, sub-agents as nested runs, and sessions as threads.

**Acceptance Scenarios**:

1. **Given** a Claude Code project folder, **When** imported, **Then** each session's messages form a trace tree that follows their parent links, with tool calls as tool runs and sidechains as sub-agent runs.
2. **Given** an imported session, **When** the developer searches for `cargo test`, **Then** the run that executed it is found and its trace shows the steps before it.
3. **Given** the same folder imported twice, **When** queried, **Then** no runs are duplicated.

---

### User Story 11 - An AI agent triages traces for me (Priority: P2)

A coding agent (e.g. Claude Code) answers "why did batch a fail?" by querying tracedb itself, reading traces in layers
that fit its context, and reporting findings with links the developer can click to check its work.

**Why this priority**: The end goal is agents debugging agents. It needs the P1 stories first, and its value is
cutting the morning triage from minutes of clicking to one question.

**Independent Test**: Give an agent only tracedb's agent tools and the question "why did batch 2026-11-05-a fail?". It
must name `fetch_url` 429s as the main cause, cite the 31 matching runs, and include UI links that open them.

**Acceptance Scenarios**:

1. **Given** a 250-run trace, **When** the agent asks for its outline, **Then** it gets one compact line per run (id, name, type, status, latency, tokens, first line of error) with payloads replaced by handle + size.
2. **Given** a failing trace, **When** the agent asks for its error path, **Then** it gets root → first error in full with everything else collapsed.
3. **Given** a payload handle, **When** the agent asks for a JSON path or character range, **Then** it gets only that slice, never more than a size cap.
4. **Given** batch a, **When** the agent asks for failure clusters, **Then** errors are grouped by normalized message with counts (e.g. "429 on fetch_url: 31").
5. **Given** a passing and a failing run on the same `task_id`, **When** the agent asks for a diff, **Then** it sees where the two diverge.
6. **Given** a finding, **When** the agent annotates runs, **Then** the tags or notes show up in the UI on those runs.

---

### User Story 12 - Find what stands out (Priority: P2)

The developer selects the slow or failing part of a chart, and tracedb ranks which fields and values differ most between
the selection and everything else (Honeycomb calls this BubbleUp).

**Why this priority**: it answers "is this a pattern?" without the developer having to guess which field to group by.
It builds on the P1 query builder.

**Independent Test**: In the scenario data, select the error spike on the batch-a error-rate chart. `name = fetch_url`
and `prompt_version = v7` must rank in the top three differences.

**Acceptance Scenarios**:

1. **Given** a selection on a chart, **When** the comparison runs, **Then** each field shows how its values are spread in the selection versus the rest, ranked by difference.
2. **Given** a ranked field, **When** the developer clicks a value, **Then** it is added to the query as a filter.

---

### Edge Cases

- **End before start**: a run's end event arrives before its start (out-of-order delivery). The run is stored and shown once the start arrives; queries see a single merged run.
- **Never-ending run**: a start with no end (the agent crashed). It stays `running` and is filterable as such; it never blocks other queries.
- **Orphan child**: a child whose parent hasn't arrived yet still shows up in its trace, attached once the parent appears.
- **Duplicate delivery**: the SDK retries a batch. Re-sent events don't create duplicate runs.
- **Huge payload**: a single input or output of several MB is accepted and viewable; list and filter speed doesn't drop because of it.
- **Malformed or unknown fields**: invalid events are rejected with a clear error and don't affect the rest of the batch. Unknown fields are kept in metadata, not dropped.
- **Disk full**: ingest refuses new events with a clear error rather than acknowledging and losing them.
- **Crash during cleanup**: an abrupt stop during background reorganization leaves data consistent; no run is lost or shown twice.
- **Empty store**: every view shows a helpful empty state that explains how to point an agent at tracedb.

## Requirements *(mandatory)*

Brackets give each requirement's source: **[Post]** = named in the SmithDB post; **[Inferred]** = implied by the post's architecture or LangSmith's product; **[Ours]** = added for the local use case.

### Functional Requirements

**Capture**

- **FR-001** [Ours]: System MUST accept traces from applications using the LangSmith SDK when only the endpoint setting is changed, with no code changes.
- **FR-002** [Post]: System MUST accept a run as multiple events (start, any number of updates, end) arriving minutes or hours apart, in any order, and present one merged run where the latest value for each field wins.
- **FR-003** [Post]: System MUST make acknowledged events visible to queries and the UI within 1 second, and MUST live-tail new runs in the runs list.
- **FR-004** [Ours]: System MUST ignore duplicate deliveries of the same event.
- **FR-005** [Ours]: System MUST reject invalid events individually with a clear reason, without rejecting the valid events in the same batch.

**Inspect**

- **FR-006** [Post]: Users MUST be able to load a full trace tree, ordered parent before child and siblings by start time, without loading inputs or outputs.
- **FR-007** [Post]: Users MUST be able to load one run's full inputs, outputs and events on demand.

**Find**

- **FR-008** [Post]: Users MUST be able to filter runs by time range, tags, status/error, latency, run type, name, model, and feedback score, combined with AND/OR/NOT.
- **FR-009** [Post]: Users MUST be able to filter on arbitrary JSON paths in metadata, inputs and outputs (equality, comparison, existence).
- **FR-010** [Post]: Users MUST be able to put conditions on the run itself, on the root run of its trace, and on *any run in the same trace*, combined in one query.
- **FR-011** [Post]: Users MUST be able to search exact phrases and terms in inputs and outputs, combined with FR-008 to FR-010.
- **FR-012** [Inferred]: System MUST return results newest first by default, paginated with a stable cursor that has no gaps or duplicates while data is arriving.
- **FR-013** [Ours]: System MUST report filter syntax errors with a message that points at the offending part.

**Measure**

- **FR-014** [Post]: Users MUST be able to aggregate filtered runs (count, error rate, latency p50/p95/p99, total tokens, total cost, average feedback score), grouped by name, model, run type, tag, or a metadata key.
- **FR-015** [Post]: Users MUST be able to show the same aggregate for two or more batches side by side.

**Follow through**

- **FR-016** [Post]: Users MUST be able to open a thread: all root runs sharing a thread id, in time order, shown as a conversation.
- **FR-017** [Post]: Users MUST be able to export all runs matching a filter, with inputs, outputs and metadata, to a JSONL file.
- **FR-020** [Post]: Users MUST be able to attach feedback (key, score, optional comment) to a finished run.
- **FR-021** [Ours]: Users MUST be able to import Claude Code session files as traces (sessions → traces and threads, tool calls → tool runs, sidechains → sub-agent runs), and importing the same files again MUST NOT create duplicates.

**Interfaces**

- **FR-022** [Ours]: System MUST offer a browser UI with a runs list (filter bar, live tail, latency/tokens/cost/status columns), a trace view (collapsible tree, timing waterfall, input/output panel with readable JSON and long-text rendering), a thread view (chat layout) and a stats view (tables plus charts).
- **FR-028** [Ours]: System MUST offer agent-facing access using the same filter language as the UI: trace outline, error path, sliced payload fetch by handle with a size cap, failure clusters, run/trace/batch diff, and annotation write-back. Every result MUST include run ids and UI links.
- **FR-025** [Ours]: Every capability in FR-001 to FR-021 MUST be usable from the UI in the same release it ships. The first release MUST include the runs list and trace view.
- **FR-026** [Ours]: Users MUST be able to build common filters by clicking values in the UI (status, tag, name, metadata value, a chart bar), without typing filter syntax.
- **FR-027** [Ours]: Every view (filtered list, trace, run, thread, stats) MUST have a shareable address that restores it exactly.
- **FR-029** [Ours]: The UI MUST offer a Scuba/Honeycomb-style query builder with one query shape (project, time range, where, group by, metrics, order, limit, optional comparison). It MUST stay in sync both ways with the text filter expression.
- **FR-030** [Ours]: System MUST maintain a field catalog of every field, including discovered metadata, input and output keys, with type and top values with counts. The catalog powers the field browser and value suggestions.
- **FR-031** [Ours]: Every query MUST be viewable as Samples, Table, Time series or Distribution without rebuilding it.
- **FR-032** [Ours]: Users MUST be able to select part of a chart and see fields ranked by how much their values differ from the rest (P2).
- **FR-023** [Ours]: System MUST offer command-line equivalents for serving, tailing, querying, importing, generating demo data, and running the benchmark.
- **FR-024** [Ours]: System MUST include a demo command that generates the overnight-eval scenario (200 tasks, planted failures, late-closing roots, 6-trace threads), runs scenario steps 4–11 and 13 as checks, and reports pass/fail plus response times.

### Quality Requirements

- **QR-001** **Local and private**: during the demo, the system makes zero outbound network connections; all data lives under one folder.
- **QR-002** **One program, nothing else to install**: no container runtime, database server or language runtime is needed to run it.
- **QR-003** **Durable**: no acknowledged event is lost when the process is killed abruptly.
- **QR-004** **Interactive** at ~2M runs / ~5 GB on a laptop, median: trace tree ≤ 50 ms, single run ≤ 50 ms, filter ≤ 100 ms, text search ≤ 500 ms, thread ≤ 150 ms; 99th percentile ≤ 5× median.
- **QR-005** **Throughput**: ≥ 20,000 events/s sustained ingest.
- **QR-006** **Light**: idle memory ≤ 200 MB; data on disk ≤ half the size of the same data as raw JSON.
- **QR-008** **Interactive querying** at ~2M runs: grouped queries and charts P50 ≤ 1 s; field and value suggestions P50 ≤ 200 ms.
- **QR-007** **Cloud-portable**: the same program can store its data on a cloud storage service and a hosted metadata database, chosen by configuration only, and passes the same test suite there.

### Key Entities *(include if feature involves data)*

- **Project**: a named bucket of traces (e.g. one agent or app).
- **Trace**: the full tree of runs for one task. Identified by a trace id; has exactly one root run.
- **Run**: one step (llm, tool, chain, retriever, agent, other). Has name, type, parent, start/end time, status, error, tags, metadata, thread id, model, token counts, cost, and large inputs, outputs and events. Its position in the tree is recorded so the whole trace or a subtree can be read in order.
- **Run Event**: one piece of a run as it arrives (start, update or end). A run is the merge of its events in arrival order.
- **Thread**: the root runs that share a thread id, forming a conversation across traces.
- **Feedback**: a key, score and optional comment attached to a run after the fact.
- **Batch**: not a separate object; a tag or metadata value (e.g. `batch=2026-11-05-a`) used to group and compare runs.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-001**: A developer goes from "nothing installed" to seeing their unmodified agent's first trace in under 5 minutes, with one install step, one start command and one environment variable.
- **SC-002**: The demo command passes every scenario check: 14 failing roots, 23 quiet failures, 31 text-search hits all on `fetch_url`, 14% `fetch_url` error rate with p95 31 s, $4.12 batch cost, a 6-trace thread, and 37 exported runs.
- **SC-003**: At ~2M runs / ~5 GB, the median response times in QR-004 hold and 99th percentiles stay within 5× median.
- **SC-004**: Ingest sustains ≥ 20,000 events/s, and a newly sent event is visible in the UI within 1 second.
- **SC-005**: In 10 out of 10 abrupt-kill-and-restart trials during ingest, zero acknowledged events are lost.
- **SC-006**: A run whose end arrives 30–120 s (and up to 40+ min) after its start shows the correct final state in the UI and in queries, both before and after background reorganization.
- **SC-007**: Zero outbound network connections are observed during the full demo.
- **SC-008**: A developer new to the tool answers the four triage questions (what failed, what failed quietly, is it a pattern, how bad) for the scenario data in under 10 minutes without reading docs beyond the UI.
- **SC-010**: Scenario steps 4–9 can be completed in the UI without typing any filter syntax.
- **SC-009**: The first deliverable is demoed in the browser (send a trace, see it live, open its tree and a payload), not through command-line output.

## Assumptions

- **Primary audience** is the overnight-eval developer. Claude Code import is a P2 story, not the headline.
- **Merge rule** (decided 2026-11-05): the latest value wins for every field, and a missing field leaves the earlier value alone. LangSmith clients re-send full values, including the whole `events` list.
- **"Any run in the trace"** (FR-010) covers the whole trace. Matching only within the subtree of a specific run is out of scope for v0.
- **Export format** is plain JSONL with run id, inputs, outputs and metadata. LangSmith dataset format can be added later.
- **Feedback**: filtering and averaging feedback that arrives with runs is in scope (P1 stories). Attaching it after the fact is P2.
- **Text search** at laptop scale (a few GB) may use a simple approach first. The response-time target, not the technique, is the requirement.
- **Single user, single machine**: no login, no access control, bound to localhost only.
- **LangSmith wire format** is public: the SDKs are open source and define it. tracedb targets pinned SDK versions (Python 0.14.17, the current release, and 0.8.18) and adds new versions through recorded-traffic tests (details in design-notes.md §4). Runs sent to an agent address instead of a project (SDK 0.14+) are accepted; their project is `<agent id>/<environment>`. Compatibility covers ingest only, not the rest of the LangSmith API (datasets, prompts, evaluators).
- **Out of scope for v0** (besides the Backlog below): OpenTelemetry ingest, run rules, run-by-run experiment comparison, raw SQL access, multi-tenant auth, replication and high availability, eval runners, prompt playground, multi-machine scale-out. Raw SQL and OpenTelemetry ingest are the likely first follow-ups.
- **Reference hardware** for performance targets is a recent developer laptop (8+ cores, 16+ GB RAM, SSD).

## Backlog

Agreed and specified, but not scheduled for v0. IDs are kept so references stay stable.

- **B1 - Delete sensitive data and enforce retention** (was Story 8, FR-018, FR-019; scenario step 12). Deferred 2026-11-05.
  - Delete runs or traces by id or filter; deleted data disappears from every list, search, thread, aggregate and export at once, and the command reports how many.
  - Retention period per project; older data stops appearing and its space is eventually reclaimed.
  - Edge cases: a run deleted while open in the UI shows as deleted on the next action; a crash mid-delete leaves data consistent.
  - Test: delete the trace with the planted API key from its trace view (with confirmation); it is gone everywhere. Set 14-day retention; older data stops appearing.
  - Why later: nothing in triage needs it, and it can be added on top of the storage design without rework (design-notes.md §7).
