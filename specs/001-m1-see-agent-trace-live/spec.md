# Feature Specification: M1 — See agent trace live

**Feature Branch**: `001-m1-see-agent-trace-live`

**Created**: 2026-11-05

**Status**: Draft

**Input**: User description: "M1 'See agent trace live' for smithling … write it descriptively: goal + why, end state as 'As a <who>, I can <what>' with general capability first and a labeled Example, work tied to end state, plus the lifecycle of one run through M1. Reference master requirement IDs instead of copying."

**Master**: [specs/000-smithling-master-plan](../000-smithling-master-plan/spec.md) (requirements of record) ·
[roadmap](../000-smithling-master-plan/roadmap.md) (progress). This folder holds the M1 details of record.

**Delivers from master**: FR-001 (JSON upload path only), FR-002, FR-003, FR-004, FR-005, FR-006, FR-007, FR-012 (basic:
newest first, paged, no filters), FR-022 (runs list + trace view), FR-025, FR-027 (list, trace, run); QR-001, QR-002, QR-003.

---

## Goal and why

Prove the whole path works end to end, thinly: **agent → smithling → durable record → browser**.

- **It unblocks every later phase.** M2 puts disk storage and the query builder under the same API and screens. M3
  adds the remaining upload format and the speed work on top of the same capture path.
- **It moves us toward the end state.** "Point an unchanged agent at it and see what it did" is the first thing a new
  user tries, and the headline of the whole product.
- **It makes UI first real.** The browser has live data from the very first phase.

## End state

*Capability first; the Example under it is the scenario we use to prove it.*

- As an **agent developer**, I can trace any agent built on the LangSmith SDK (LangChain, LangGraph, or plain
  instrumented code) into smithling by changing only its endpoint setting, and see each step in the browser within 1 s.
  Steps still running show as running and update when they finish, however long that takes.
  - *Example*: the research agent from the master scenario, with a 40-minute root step that flips from `running` to `ok`.
- As an agent developer, I can watch new runs arrive in a list, newest first, without refreshing, and page back
  through older ones.
  - *Example*: an eval batch of 200 tasks streaming in while the list stays scrolled to the top.
- As an agent developer, I can open any trace and see its full tree and timing waterfall at once, then click any step
  to read its complete inputs and outputs.
  - *Example*: a 250-step trace; clicking a `fetch_url` step shows its 40 KB response.
- As an agent developer, I can send a link to any trace or step, and opening it shows the same view.
- As an agent developer, I can have smithling crash or be killed at any moment and lose nothing it acknowledged.
  - *Example*: `kill -9` while 200 tasks are uploading, then restart: every acknowledged step is back.
- As a **test user** without an agent, I can generate realistic traces (deep trees, late-finishing steps, large
  payloads, some errors) and explore them in the browser.
  - *Example*: generate 100 traces and click through them.
- As a **maintainer**, I can trust all of the above stays true: every change is checked automatically for crash safety,
  compatibility with recorded SDK traffic, and zero outbound connections.

## Lifecycle of one run in M1

The `fetch_url` step from the master scenario, from the agent to the browser. The last column names the M1 work that
makes each stage happen.

| # | When | What happens | M1 work |
|---|---|---|---|
| 1 | agent starts | The SDK asks smithling what it supports. smithling answers "send plain JSON batches". | Capability handshake |
| 2 | 02:14:07 | The SDK sends the step's **start** (name, inputs, tags, metadata) in a batch. smithling checks each item on its own, numbers it, and records it durably; only then does it confirm. | Accept SDK uploads; per-item validation; durable log |
| 3 | +0 s | The step is held in memory and appears in the open runs list as `running` within 1 s. | In-memory store; live list |
| 4 | +5 s | The SDK retries the same batch (a network hiccup). The repeat is recognized and ignored. | Duplicate detection |
| 5 | 02:14:38 | The **update** arrives: end time, error, outputs. smithling merges it into the same run (latest value wins), so status becomes `error` and latency 31 s. The open screens update. | Merge rule; live updates |
| 6 | 08:00 | The developer opens the trace. The tree and waterfall show every step in order without loading any payloads. Clicking `fetch_url` loads its full input and output. | Trace view; run panel |
| 7 | anytime | smithling is killed and restarted. It replays its durable log, and the run is back exactly as before. | Replay on start |
| 8 | every change | Automated checks re-run the crash test, the recorded-SDK-traffic test and the no-outbound check. | Automated gates |

## User Scenarios & Testing *(mandatory)*

### User Story 1 - Trace an unmodified agent and watch it live (Priority: P1)

An agent developer starts smithling, sets one endpoint variable for their existing agent, and watches its runs appear and
update in the browser as the agent works.

**Why this priority**: it is the headline capability, and nothing else can be shown without data arriving.

**Independent Test**: start smithling on an empty data folder, run an unmodified LangGraph example agent with the endpoint
set, and confirm in the browser that its runs appear within 1 s and update when they finish.

**Acceptance Scenarios**:

1. **Given** smithling is running, **When** an agent using the LangSmith SDK runs with only its endpoint changed, **Then** every step of its trace appears in the runs list, with no code change and no extra settings.
2. **Given** a step that started 40 minutes ago, **When** its end arrives, **Then** the list and any open trace show its final status, end time and outputs without a manual refresh.
3. **Given** a step that received several updates, **When** viewed, **Then** it shows one merged state where the latest value of each field wins and missing fields keep earlier values.
4. **Given** the list is open and new runs arrive, **When** the developer is at the top, **Then** new runs appear at the top; **When** they have scrolled down, **Then** their place is kept and a "new runs" indicator appears.
5. **Given** more runs than one page, **When** the developer pages back, **Then** older runs continue newest first with no repeats.
6. **Given** a batch with one malformed item, **When** it is uploaded, **Then** the valid items are accepted and the malformed one is reported with a reason.

---

### User Story 2 - Inspect a trace and any step in it (Priority: P1)

The developer opens a trace, sees its whole structure and timing, and reads any step's full content.

**Why this priority**: "what happened in this one?" is the core debugging question, and it is half of the M1 demo.

**Independent Test**: load a 250-step trace with 40 KB payloads, open it, and confirm the tree and waterfall render
before any payload loads, then open one step and read its full payload.

**Acceptance Scenarios**:

1. **Given** a trace of 250 steps, **When** opened, **Then** the tree (name, type, status, duration, tokens) appears in parent-before-child order with a matching timing waterfall, without loading any inputs or outputs.
2. **Given** an open trace, **When** a step is selected, **Then** its full inputs and outputs appear, with structured data readable and long text wrapped.
3. **Given** a trace whose root is still running, **When** opened, **Then** finished steps appear, the root shows as running, and new steps appear as they arrive.
4. **Given** a link to a trace or a selected step, **When** opened in a new tab, **Then** the same trace and step are shown.
5. **Given** a large tree, **When** the developer collapses or expands a branch, **Then** the tree and waterfall stay aligned.

---

### User Story 3 - Never lose acknowledged data (Priority: P1)

The developer can rely on everything smithling confirmed being there after any crash or restart.

**Why this priority**: a trace store that loses data is not trusted again; durability is cheapest to build in from the start.

**Independent Test**: upload continuously, kill smithling abruptly at random points, restart, and compare the stored runs
with every confirmed upload, over 10 rounds.

**Acceptance Scenarios**:

1. **Given** uploads in progress, **When** smithling is killed abruptly and restarted, **Then** every confirmed item is present and merged correctly.
2. **Given** an upload that was not confirmed before the kill, **When** the SDK retries it after restart, **Then** it is stored once.
3. **Given** a restart, **When** smithling comes back, **Then** it is ready to accept uploads and show data within 30 s at the M1 capacity limit (decided 2026-11-05, design §11.4; measured 0.83 s at 500k runs).

---

### User Story 4 - Explore realistic traces without an agent (Priority: P2)

A test user generates realistic traces and explores them, without writing or running an agent.

**Why this priority**: it lets anyone try smithling in a minute, and gives UI work realistic data. It is not needed for
the core demo, which uses a real agent.

**Independent Test**: generate 100 traces, then confirm in the browser that they include deep trees, late-finishing
steps, errors and large payloads.

**Acceptance Scenarios**:

1. **Given** an empty smithling, **When** the test user generates N traces, **Then** N traces appear, uploaded the same way an agent would send them.
2. **Given** generated data, **When** browsed, **Then** it includes depth 3–8, 20–300 steps per trace, about 5% errors, about 10% of steps finishing 30–120 s late, and payloads of 1–50 KB.
3. **Given** the same generation settings and seed, **When** run twice, **Then** the same shapes are produced.

---

### Edge Cases

- **End before start**: an update arrives before the start; once the start arrives, the run shows one merged state.
- **Orphan step**: a child arrives before its parent; it shows in its trace and attaches once the parent appears.
- **Never-ending step**: a start with no end stays `running` indefinitely and doesn't block anything.
- **Duplicate upload**: a retried batch is stored once.
- **Malformed item**: rejected on its own with a reason; the rest of the batch is accepted.
- **Huge payload**: a single input or output up to 20 MB is accepted and viewable; larger ones are rejected with a reason.
- **Agent address instead of a project** (SDK 0.14+): the run is accepted, and its project is `<agent id>/<environment>` from the address.
- **Unknown fields**: kept rather than dropped; ignored fields (such as attachments) are acknowledged and noted.
- **Memory limit reached** (M1 keeps data in memory): smithling refuses new uploads with a clear reason instead of confirming and then losing them.
- **Disk full**: uploads are refused with a clear reason; nothing confirmed is lost.
- **Empty store**: the runs list explains how to point an agent at smithling.
- **Agent asks for an unsupported upload format**: the capability answer steers the SDK to plain JSON batches; a request in another format gets a clear "unsupported in this version" reply.

## Requirements *(mandatory)*

Each requirement names the master requirement it delivers. The master text is not repeated here.

### Functional Requirements

- **M1-FR-01** (→ FR-001, JSON path): System MUST accept traces from LangSmith SDK clients with only the endpoint changed. It MUST answer the SDK's capability check so the client uses plain JSON batches, and accept single-run creates and updates.
- **M1-FR-02** (→ FR-004, FR-005): System MUST validate each uploaded item on its own, store each valid item once even if re-sent, and report rejected items with reasons.
- **M1-FR-03** (→ FR-002): System MUST merge all events for a run, in arrival order, with the latest value per field winning and missing fields leaving earlier values unchanged.
- **M1-FR-04** (→ QR-003): System MUST confirm an upload only after it is durably recorded, and MUST restore every confirmed event after any restart.
- **M1-FR-05** (→ FR-003): System MUST show confirmed events in the browser within 1 s, and push new and updated runs to open screens.
- **M1-FR-06** (→ FR-012, basic): Users MUST be able to list root runs newest first, page back without repeats, and narrow by project and by status.
- **M1-FR-07** (→ FR-006): Users MUST be able to open a trace and see the full tree and a timing waterfall without loading payloads.
- **M1-FR-08** (→ FR-007): Users MUST be able to open one step's full inputs, outputs and events on demand.
- **M1-FR-09** (→ FR-022, FR-025, FR-027): The browser MUST provide the runs list, trace view and step panel, each with a shareable address, plus an empty state that explains setup.
- **M1-FR-10** (master SC-002 support): System MUST provide a command that generates realistic traces through the same upload path agents use.
- **M1-FR-11** (→ QR-001): System MUST make no outbound connections and keep all state under one data folder.
- **M1-FR-12** (→ QR-002): System MUST run as one program with nothing else to install.
- **M1-FR-13** (constitution VI): Automated checks MUST run on every change: the crash test, the recorded-SDK-traffic test and the no-outbound check.

### Key Entities

- **Run event**: one message about a step (start or update), numbered on arrival. See master data model.
- **Run**: one step, the merge of its events: identity and tree position, name, type, timing, status, tags, metadata, tokens, and payload (inputs, outputs, events).
- **Trace**: all runs sharing a trace id, shown as a tree; its root is the run without a parent.
- **Project**: the bucket a run belongs to, from the SDK's project setting.

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-01**: A developer with an existing LangSmith-instrumented agent sees their first trace in the browser within 5 minutes of downloading smithling, changing only one setting.
- **SC-02**: 99% of confirmed events are visible in the browser within 1 s of confirmation.
- **SC-03**: A 250-step trace opens with tree and waterfall in under 1 s on the reference laptop.
- **SC-04**: In 10 of 10 abrupt-kill rounds during upload, zero confirmed events are missing after restart.
- **SC-05**: Zero outbound network connections during the full M1 demo.
- **SC-06**: Recorded traffic from the pinned SDK versions replays with 100% of items accepted and runs identical to the recorded expectation.
- **SC-07**: smithling sustains at least 5,000 events/s on the reference laptop (M3 raises this to the master's 20,000).

## Assumptions

- **Storage is in memory plus a durable log.** M1 doesn't write the columnar files; that's M2. Capacity is limited by
  RAM. The M1 limit is set at about 500k runs or a 2 GB memory budget, whichever comes first, beyond which uploads are
  refused (see Edge Cases).
- **Speed targets** are M1-sized (SC-03, SC-07). The master speed targets apply from M3.
- **Upload formats**: plain JSON batches and single creates/updates are supported. The multipart format, feedback and
  attachments arrive in M3; attachments are acknowledged and noted, not stored.
- **SDK versions**: compatibility is checked against pinned Python and JS SDK versions from recorded traffic. The
  pins are Python `langsmith` **0.14.17** (current as of 2026-11-05) and **0.8.18** (older agents still run it); both were read for the design.
- **Filters** in M1 are project and status only. The query builder is M2.
- **Single user**, localhost only, no login.
- **Out of scope for M1**: disk columnar storage, metastore, query builder and filters beyond project/status, text
  search, stats and charts, threads, export, feedback, compaction, multipart, Claude Code import, agent tools.
